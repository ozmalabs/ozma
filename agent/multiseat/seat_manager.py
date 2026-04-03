# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Seat manager — orchestrates multi-seat lifecycle.

Discovers displays and input devices, creates one Seat per display,
assigns input groups to seats, manages the full lifecycle.

Usage:
  manager = SeatManager(controller_url="http://localhost:7380")
  await manager.start()   # discovers, creates, starts all seats
  ...
  await manager.stop()    # clean shutdown
"""

from __future__ import annotations

import asyncio
import json
import logging
import platform
from pathlib import Path
from typing import Any

from .display_backend import DisplayBackend, DisplayInfo
from .encoder_allocator import EncoderAllocator, EncoderHints
from .virtual_display import VirtualDisplayManager
from .game_launcher import GameLauncher
from .gpu_inventory import GPUInventory
from .hotplug import HotplugMonitor
from .input_router import InputGroup, InputRouterBackend
from .audio_backend import SeatAudioBackend
from .monitoring import MonitoringServer
from .seat import Seat
from .seat_profiles import SeatProfile, get_profile, WORKSTATION

log = logging.getLogger("ozma.agent.multiseat.seat_manager")


class SeatManager:
    """
    Orchestrator for multi-seat on a single PC.

    Discovers displays and input devices, creates seats, manages lifecycle.
    Each seat registers as an independent ozma node — the controller sees
    N machines where there is physically one.
    """

    def __init__(
        self,
        controller_url: str = "",
        base_udp_port: int = 7331,
        base_api_port: int = 7382,
        seat_count: int | None = None,
        seat_config_path: str | None = None,
        profile_name: str = "workstation",
        machine_name: str = "",
    ) -> None:
        self._controller_url = controller_url
        self._base_udp_port = base_udp_port
        self._base_api_port = base_api_port
        self._seat_count = seat_count  # None = auto-detect from displays
        self._seat_config_path = seat_config_path
        self._profile = get_profile(profile_name)
        self._machine_name = machine_name or platform.node()

        self._seats: list[Seat] = []
        self._seat_tasks: list[asyncio.Task] = []
        self._display_backend: DisplayBackend | None = None
        self._input_backend: InputRouterBackend | None = None
        self._audio_backend: SeatAudioBackend | None = None
        self._gpu_inventory: GPUInventory = GPUInventory()
        self._encoder_allocator: EncoderAllocator | None = None
        self._game_launcher: GameLauncher | None = None
        self._hotplug: HotplugMonitor | None = None
        self._monitoring: MonitoringServer | None = None
        self._vdm: VirtualDisplayManager | None = None
        self._displays: list[DisplayInfo] = []
        self._input_groups: list[InputGroup] = []
        self._stop_event = asyncio.Event()

    @property
    def seats(self) -> list[Seat]:
        return list(self._seats)

    @property
    def seat_count(self) -> int:
        return len(self._seats)

    @property
    def encoder_allocator(self) -> EncoderAllocator | None:
        return self._encoder_allocator

    @property
    def game_launcher(self) -> GameLauncher | None:
        return self._game_launcher

    @property
    def hotplug(self) -> HotplugMonitor | None:
        return self._hotplug

    @property
    def virtual_display_manager(self) -> VirtualDisplayManager | None:
        return self._vdm

    async def start(self) -> None:
        """
        Full startup sequence:
        1. Initialize platform backends
        2. Enumerate displays
        3. Enumerate input groups (USB topology)
        4. Load or create seat config
        5. Create seats
        6. Start all seats concurrently
        """
        log.info("SeatManager starting on %s (%s)", self._machine_name, platform.system())

        # Initialize platform-specific backends
        self._init_backends()

        # Initialize virtual display manager (auto-detects driver)
        self._vdm = VirtualDisplayManager()
        if self._vdm.available:
            log.info("Virtual display driver: %s", self._vdm.driver_name)
        else:
            log.info("No virtual display driver — extra seats need physical displays or dummy plugs")

        # Discover GPUs and create encoder allocator
        await self._gpu_inventory.discover()
        self._encoder_allocator = EncoderAllocator(self._gpu_inventory)

        # Enumerate displays
        self._displays = self._display_backend.enumerate()
        log.info("Found %d displays", len(self._displays))

        # Enumerate input groups
        self._input_groups = self._input_backend.enumerate_groups()
        log.info("Found %d input groups", len(self._input_groups))

        # Determine seat count
        target_count = self._seat_count or len(self._displays)
        if target_count < 1:
            log.warning("No displays found and no seat count specified — creating 1 seat")
            target_count = 1

        # Create virtual displays if we need more seats than physical displays
        while len(self._displays) < target_count:
            log.info("Creating virtual display %d (need %d, have %d)",
                     len(self._displays), target_count, len(self._displays))
            vd = self._display_backend.create_virtual(
                self._profile.capture_width,
                self._profile.capture_height,
            )
            if vd:
                self._displays.append(vd)
            else:
                log.warning("Cannot create virtual display — capping at %d seats",
                            len(self._displays))
                target_count = len(self._displays)
                break

        # Load seat config if provided
        seat_configs = self._load_seat_config() if self._seat_config_path else None

        # Create seats
        for i in range(target_count):
            display = self._displays[i] if i < len(self._displays) else None

            # Determine seat name
            if seat_configs and i < len(seat_configs):
                name = seat_configs[i].get("name", f"{self._machine_name}-seat-{i}")
                profile_name = seat_configs[i].get("profile", self._profile.name)
                profile = get_profile(profile_name)
            else:
                name = f"{self._machine_name}-seat-{i}"
                profile = self._profile

            # Allocate encoder for this seat
            encoder_hints = EncoderHints(
                resolution=(profile.capture_width, profile.capture_height),
                fps=profile.capture_fps,
            )
            if seat_configs and i < len(seat_configs):
                gaming_gpu = seat_configs[i].get("gaming_gpu")
                if gaming_gpu is not None:
                    encoder_hints.gaming_gpu_index = int(gaming_gpu)
                if seat_configs[i].get("prefer_quality"):
                    encoder_hints.prefer_quality = True
                if seat_configs[i].get("codec"):
                    encoder_hints.codec = seat_configs[i]["codec"]

            encoder_session = self._encoder_allocator.allocate(name, encoder_hints)

            seat = Seat(
                name=name,
                seat_index=i,
                display_index=display.index if display else i,
                udp_port=self._base_udp_port + i,
                api_port=self._base_api_port + i,
                capture_fps=profile.capture_fps,
                capture_width=profile.capture_width,
                capture_height=profile.capture_height,
                encoder_args=encoder_session.ffmpeg_args,
            )
            seat.display = display
            self._seats.append(seat)

        # Create audio sinks for each seat
        for seat in self._seats:
            sink = await self._audio_backend.create_sink(seat.name)
            seat.audio_sink = sink

        # Assign input groups to seats
        self._assign_inputs()

        # Start all seats concurrently
        log.info("Starting %d seats", len(self._seats))
        for seat in self._seats:
            task = asyncio.create_task(
                seat.start(self._controller_url),
                name=f"seat-{seat.name}",
            )
            self._seat_tasks.append(task)

        # Start monitoring server
        self._monitoring = MonitoringServer(self)
        await self._monitoring.start()

        # Start game launcher (discovers game libraries in background)
        self._game_launcher = GameLauncher(self)
        asyncio.create_task(
            self._game_launcher.discover_games(),
            name="game-discovery",
        )

        # Start USB hotplug monitor
        self._hotplug = HotplugMonitor(self)
        await self._hotplug.start()

        log.info("SeatManager running: %d seats on %s",
                 len(self._seats), self._machine_name)

        # Wait for stop signal
        await self._stop_event.wait()

    async def stop(self) -> None:
        """Stop all seats and clean up resources."""
        log.info("SeatManager stopping")
        self._stop_event.set()

        # Stop hotplug monitor
        if self._hotplug:
            await self._hotplug.stop()
            self._hotplug = None

        # Stop games on all seats
        if self._game_launcher:
            for seat in self._seats:
                await self._game_launcher.stop(seat)
            self._game_launcher = None

        # Stop monitoring server
        if self._monitoring:
            await self._monitoring.stop()
            self._monitoring = None

        # Stop all seats and release encoder sessions
        for seat in self._seats:
            await seat.stop()
            if self._encoder_allocator:
                self._encoder_allocator.release(seat.name)

        # Cancel seat tasks
        for task in self._seat_tasks:
            task.cancel()
            try:
                await task
            except asyncio.CancelledError:
                pass

        # Destroy audio sinks
        if self._audio_backend:
            for seat in self._seats:
                await self._audio_backend.destroy_sink(seat.name)

        # Clean up input assignments
        if self._input_backend:
            for group in self._input_groups:
                self._input_backend.unassign(group)

        # Destroy virtual displays via VDM (preferred) or display backend
        if self._vdm and self._vdm.available:
            count = await self._vdm.remove_all()
            if count:
                log.info("Cleaned up %d virtual display(s) via %s",
                         count, self._vdm.driver_name)
        elif self._display_backend:
            for display in self._displays:
                if display.virtual:
                    self._display_backend.destroy_virtual(display)

        self._seats.clear()
        self._seat_tasks.clear()
        log.info("SeatManager stopped")

    def _init_backends(self) -> None:
        """Initialize platform-specific backends."""
        system = platform.system()

        if system == "Linux":
            from .display_linux import LinuxDisplayBackend
            from .input_linux import LinuxInputBackend
            from .audio_linux import LinuxAudioBackend

            self._display_backend = LinuxDisplayBackend()
            self._input_backend = LinuxInputBackend()
            self._audio_backend = LinuxAudioBackend()

        elif system == "Windows":
            from .display_windows import WindowsDisplayBackend
            from .input_windows import WindowsInputBackend
            from .audio_windows import WindowsAudioBackend

            self._display_backend = WindowsDisplayBackend()
            self._input_backend = WindowsInputBackend()
            self._audio_backend = WindowsAudioBackend()

        else:
            log.warning("Unsupported platform %s — using stubs", system)
            self._display_backend = _StubDisplayBackend()
            self._input_backend = _StubInputBackend()
            self._audio_backend = _StubAudioBackend()

    def _assign_inputs(self) -> None:
        """
        Assign input groups to seats.

        Strategy:
        1. Groups with keyboard+mouse → assign to seats in order
        2. Groups with only gamepad → assign to the next seat that
           doesn't have a gamepad yet
        3. Remaining devices go to seat 0 (the primary seat)
        """
        if not self._input_groups or not self._seats:
            return

        # Separate full input groups from gamepad-only groups
        full_groups = [g for g in self._input_groups if g.has_input]
        gamepad_groups = [g for g in self._input_groups if not g.has_input and g.gamepads]

        # Assign full groups to seats in order
        for i, group in enumerate(full_groups):
            if i < len(self._seats):
                seat = self._seats[i]
                seat.input_devices = group.all_devices
                self._input_backend.assign(group, seat)
            else:
                log.debug("More input groups (%d) than seats (%d) — group %s unassigned",
                          len(full_groups), len(self._seats), group.hub_path)

        # Assign gamepad groups to seats that don't have one
        gamepad_idx = 0
        for group in gamepad_groups:
            if gamepad_idx < len(self._seats):
                seat = self._seats[gamepad_idx]
                seat.input_devices.extend(group.gamepads)
                self._input_backend.assign(group, seat)
                gamepad_idx += 1

        log.info("Input assignment: %d full groups, %d gamepad groups → %d seats",
                 len(full_groups), len(gamepad_groups), len(self._seats))

    def _load_seat_config(self) -> list[dict] | None:
        """
        Load seat configuration from a JSON file.

        Format:
        [
            {"name": "gaming-seat", "profile": "gaming", "display": "HDMI-1"},
            {"name": "work-seat", "profile": "workstation", "display": "DP-2"}
        ]
        """
        if not self._seat_config_path:
            return None

        path = Path(self._seat_config_path)
        if not path.exists():
            log.warning("Seat config not found: %s", path)
            return None

        try:
            with open(path) as f:
                config = json.load(f)
            if isinstance(config, list):
                log.info("Loaded seat config: %d seats from %s", len(config), path)
                return config
            log.warning("Seat config is not a list: %s", path)
            return None
        except Exception as e:
            log.warning("Failed to load seat config: %s", e)
            return None

    async def rebalance_encoders(self, seat_name: str | None = None,
                                gaming_gpu_index: int | None = None) -> list[str]:
        """
        Rebalance encoder allocations.

        Call when a game launches/exits on a seat to update GPU affinity hints.
        If seat_name and gaming_gpu_index are provided, update that seat's hints
        before rebalancing.

        Returns list of seat names that need capture restart.
        """
        if not self._encoder_allocator:
            return []

        if seat_name and gaming_gpu_index is not None:
            session = self._encoder_allocator.sessions.get(seat_name)
            if session:
                session.hints.gaming_gpu_index = gaming_gpu_index

        reassigned = self._encoder_allocator.rebalance()

        # Update ffmpeg args on reassigned seats
        for name in reassigned:
            for seat in self._seats:
                if seat.name == name:
                    new_args = self._encoder_allocator.get_ffmpeg_args(name)
                    seat.encoder_args = new_args
                    # Seat will need to restart its capture with the new args
                    log.info("Seat %s encoder changed — capture restart needed", name)
                    break

        return reassigned

    async def create_hotplug_seat(
        self,
        name: str,
        seat_index: int,
        input_group: InputGroup,
    ) -> Seat | None:
        """
        Create a new seat dynamically when USB devices are hotplugged.

        Called by the HotplugMonitor when a new keyboard+mouse group appears.
        Finds or creates a display, allocates an encoder, and starts the seat.
        """
        # Find next available display or create virtual
        display: DisplayInfo | None = None
        used_display_indices = {s.display_index for s in self._seats}
        for d in self._displays:
            if d.index not in used_display_indices:
                display = d
                break

        if not display and self._display_backend:
            display = self._display_backend.create_virtual(
                self._profile.capture_width,
                self._profile.capture_height,
            )
            if display:
                self._displays.append(display)

        # Allocate encoder
        encoder_hints = EncoderHints(
            resolution=(self._profile.capture_width, self._profile.capture_height),
            fps=self._profile.capture_fps,
        )
        encoder_session = self._encoder_allocator.allocate(name, encoder_hints) if self._encoder_allocator else None

        # Determine ports
        udp_port = self._base_udp_port + seat_index
        api_port = self._base_api_port + seat_index

        seat = Seat(
            name=name,
            seat_index=seat_index,
            display_index=display.index if display else seat_index,
            udp_port=udp_port,
            api_port=api_port,
            capture_fps=self._profile.capture_fps,
            capture_width=self._profile.capture_width,
            capture_height=self._profile.capture_height,
            encoder_args=encoder_session.ffmpeg_args if encoder_session else [],
        )
        seat.display = display
        seat.input_devices = input_group.all_devices

        # Create audio sink
        if self._audio_backend:
            sink = await self._audio_backend.create_sink(name)
            seat.audio_sink = sink

        # Assign input
        if self._input_backend:
            self._input_backend.assign(input_group, seat)

        self._seats.append(seat)

        # Start seat in background
        task = asyncio.create_task(
            seat.start(self._controller_url),
            name=f"seat-{seat.name}",
        )
        self._seat_tasks.append(task)

        log.info("Hotplug seat created: %s (index=%d, display=%s, udp=%d)",
                 name, seat_index,
                 display.name if display else "none",
                 udp_port)

        return seat

    async def destroy_hotplug_seat(self, seat_name: str) -> bool:
        """
        Destroy a seat that was created by hotplug.

        Called by the HotplugMonitor when devices are removed and the
        grace period expires. Stops the seat, releases resources.
        """
        seat = None
        seat_idx = None
        for i, s in enumerate(self._seats):
            if s.name == seat_name:
                seat = s
                seat_idx = i
                break

        if not seat:
            log.warning("Cannot destroy seat %s — not found", seat_name)
            return False

        # Stop any running game
        if self._game_launcher:
            await self._game_launcher.stop(seat)

        # Stop the seat
        await seat.stop()

        # Release encoder
        if self._encoder_allocator:
            self._encoder_allocator.release(seat_name)

        # Destroy audio sink
        if self._audio_backend:
            await self._audio_backend.destroy_sink(seat_name)

        # Destroy virtual display if applicable
        if seat.display and seat.display.virtual and self._display_backend:
            self._display_backend.destroy_virtual(seat.display)
            if seat.display in self._displays:
                self._displays.remove(seat.display)

        # Remove from seat list
        self._seats.pop(seat_idx)

        # Cancel corresponding task
        for task in self._seat_tasks:
            if task.get_name() == f"seat-{seat_name}":
                task.cancel()
                try:
                    await task
                except asyncio.CancelledError:
                    pass
                self._seat_tasks.remove(task)
                break

        log.info("Hotplug seat destroyed: %s", seat_name)
        return True

    def get_seat(self, seat_name: str) -> Seat | None:
        """Look up a seat by name."""
        for seat in self._seats:
            if seat.name == seat_name:
                return seat
        return None

    def to_dict(self) -> dict:
        """Serialize manager state for diagnostics."""
        result = {
            "machine": self._machine_name,
            "seat_count": len(self._seats),
            "displays": len(self._displays),
            "input_groups": len(self._input_groups),
            "profile": self._profile.name,
            "seats": [s.to_dict() for s in self._seats],
        }
        if self._encoder_allocator:
            result["encoders"] = self._encoder_allocator.to_dict()
        if self._gpu_inventory.gpus:
            result["gpus"] = self._gpu_inventory.to_dict()
        if self._game_launcher:
            result["games"] = self._game_launcher.to_dict()
        if self._hotplug:
            result["hotplug"] = self._hotplug.to_dict()
        if self._vdm:
            result["virtual_display"] = self._vdm.to_dict()
        return result


# ── Stub backends for unsupported platforms ──────────────────────────────────

class _StubDisplayBackend(DisplayBackend):
    def enumerate(self) -> list[DisplayInfo]:
        return [DisplayInfo(index=0, name="default", width=1920, height=1080,
                            x_screen=":0", primary=True)]

    def create_virtual(self, width=1920, height=1080, name="") -> DisplayInfo | None:
        return None

    def destroy_virtual(self, display: DisplayInfo) -> bool:
        return False


class _StubInputBackend(InputRouterBackend):
    def enumerate_groups(self) -> list[InputGroup]:
        return [InputGroup(hub_path="default",
                           keyboards=["default-keyboard"],
                           mice=["default-mouse"])]

    def assign(self, group: InputGroup, seat: Any) -> bool:
        return True

    def unassign(self, group: InputGroup) -> bool:
        return True


class _StubAudioBackend(SeatAudioBackend):
    async def create_sink(self, seat_name: str) -> str | None:
        return None

    async def destroy_sink(self, seat_name: str) -> bool:
        return True

    async def assign_output(self, seat_name: str, device: str) -> bool:
        return False

    async def list_sinks(self) -> list[dict]:
        return []
