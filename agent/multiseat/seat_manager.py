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
from .isolation import IsolationManager
from .hotplug import HotplugMonitor
from .input_router import InputGroup, InputRouterBackend
from .audio_backend import SeatAudioBackend
from .monitoring import MonitoringServer
from .seat import Seat
from .seat_profiles import SeatProfile, get_profile, WORKSTATION

log = logging.getLogger("ozma.agent.multiseat.seat_manager")
log_config = logging.getLogger("ozma.agent.multiseat.config")

# Default path for persisted seat config
_DEFAULT_CONFIG_DIR = Path.home() / ".config" / "ozma"
_DEFAULT_CONFIG_FILE = _DEFAULT_CONFIG_DIR / "seat_config.json"


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
        self._isolation_manager: IsolationManager = IsolationManager()
        self._hotplug: HotplugMonitor | None = None
        self._monitoring: MonitoringServer | None = None
        self._vdm: VirtualDisplayManager | None = None
        self._displays: list[DisplayInfo] = []
        self._input_groups: list[InputGroup] = []
        self._stop_event = asyncio.Event()
        self._config_ws_task: asyncio.Task | None = None
        self._scaling_lock = asyncio.Lock()  # serialize scale-up/down

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
    def isolation_manager(self) -> IsolationManager:
        return self._isolation_manager

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

        try:
            await self._start_inner()
        except Exception as e:
            log.error("SeatManager startup failed: %s", e, exc_info=True)
            raise

    async def _start_inner(self) -> None:
        print("[OZMA DEBUG] _start_inner begin", flush=True)
        # Initialize platform-specific backends
        self._init_backends()
        print("[OZMA DEBUG] backends initialized", flush=True)

        # Initialize virtual display manager (auto-detects driver)
        self._vdm = VirtualDisplayManager()
        if self._vdm.available:
            log.info("Virtual display driver: %s", self._vdm.driver_name)
        else:
            log.info("No virtual display driver — extra seats need physical displays or dummy plugs")

        # Discover GPUs and create encoder allocator
        try:
            await self._gpu_inventory.discover()
        except Exception as e:
            log.warning("GPU discovery failed: %s — continuing without encoder optimization", e)
        print(f"[OZMA DEBUG] GPU discovery returned, {len(self._gpu_inventory.gpus)} GPUs", flush=True)
        log.info("GPU discovery complete: %d GPUs", len(self._gpu_inventory.gpus))
        try:
            self._encoder_allocator = EncoderAllocator(self._gpu_inventory)
        except Exception as e:
            print(f"[OZMA DEBUG] EncoderAllocator init failed: {e}", flush=True)
            raise
        print("[OZMA DEBUG] Encoder allocator ready", flush=True)

        # Enumerate displays
        print("[OZMA DEBUG] Enumerating displays...", flush=True)
        try:
            self._displays = self._display_backend.enumerate()
        except Exception as e:
            print(f"[OZMA DEBUG] Display enum failed: {e}", flush=True)
            log.warning("Display enumeration failed: %s — creating default display", e)
            self._displays = [DisplayInfo(index=0, name="default", width=1920, height=1080)]
        log.info("Found %d displays: %s", len(self._displays),
                 ", ".join(d.name for d in self._displays))

        # Enumerate input groups
        print("[OZMA DEBUG] Enumerating input groups...", flush=True)
        try:
            self._input_groups = self._input_backend.enumerate_groups()
        except Exception as e:
            print(f"[OZMA DEBUG] Input enum failed: {e}", flush=True)
            log.warning("Input enumeration failed: %s", e)
            self._input_groups = []
        print(f"[OZMA DEBUG] Found {len(self._input_groups)} input groups", flush=True)
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
            print(f"[OZMA DEBUG] Creating seat {name} (index={i})", flush=True)

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
            print(f"[OZMA DEBUG] Seat {name} created", flush=True)

        print(f"[OZMA DEBUG] {len(self._seats)} seats created, setting up audio...", flush=True)
        # Create audio sinks for each seat
        for seat in self._seats:
            try:
                sink = await self._audio_backend.create_sink(seat.name)
                seat.audio_sink = sink
                print(f"[OZMA DEBUG] Audio sink for {seat.name}: {sink}", flush=True)
            except Exception as e:
                print(f"[OZMA DEBUG] Audio sink failed for {seat.name}: {e}", flush=True)
                log.warning("Seat %s: audio sink creation failed: %s", seat.name, e)

        # Set up per-seat isolation based on profile
        for seat in self._seats:
            isolation_name = self._profile.isolation
            if seat_configs and seat.seat_index < len(seat_configs):
                isolation_name = seat_configs[seat.seat_index].get(
                    "isolation", isolation_name,
                )
            if isolation_name != "none":
                try:
                    ctx = await self._isolation_manager.setup_seat(
                        seat.name, isolation_name, seat.seat_index,
                    )
                    log.info("Seat %s: isolation=%s", seat.name, isolation_name)
                except (ValueError, RuntimeError) as e:
                    log.warning("Seat %s: isolation setup failed (%s), using none",
                                seat.name, e)

        # Assign input groups to seats
        self._assign_inputs()

        # Start all seats concurrently
        log.info("Starting %d seats", len(self._seats))
        for seat in self._seats:
            async def _safe_start(s=seat):
                try:
                    await s.start(self._controller_url)
                except Exception as e:
                    print(f"[OZMA DEBUG] Seat {s.name} crashed: {e}", flush=True)
                    log.error("Seat %s crashed: %s", s.name, e, exc_info=True)

            task = asyncio.create_task(
                _safe_start(),
                name=f"seat-{seat.name}",
            )
            self._seat_tasks.append(task)

        # Monitoring, game launcher, and hotplug deferred until seat is stable
        print("[OZMA DEBUG] Skipping optional services for now...", flush=True)

        log.info("SeatManager running: %d seats on %s",
                 len(self._seats), self._machine_name)
        print(f"[OZMA DEBUG] SeatManager fully started — {len(self._seats)} seats", flush=True)

        # Config WS disabled for initial testing
        # if self._controller_url:
        #     self._config_ws_task = asyncio.create_task(
        #         self._connect_config_ws(), name="config-ws",
        #     )

        # Wait for stop signal
        print("[OZMA DEBUG] About to await stop signal...", flush=True)
        import sys as _sys
        _sys.stdout.flush()
        _sys.stderr.flush()
        try:
            await self._stop_event.wait()
        except Exception as e:
            print(f"[OZMA DEBUG] stop_event.wait failed: {e}", flush=True)
        print("[OZMA DEBUG] Stop signal received", flush=True)

    async def stop(self) -> None:
        """Stop all seats and clean up resources."""
        log.info("SeatManager stopping")
        self._stop_event.set()

        # Cancel config WS task
        if self._config_ws_task:
            self._config_ws_task.cancel()
            try:
                await self._config_ws_task
            except asyncio.CancelledError:
                pass
            self._config_ws_task = None

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

        # Tear down per-seat isolation
        for seat in self._seats:
            await self._isolation_manager.teardown_seat(seat.name)

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

    # ── Config WebSocket (controller push) ──────────────────────────────────

    def _node_id(self) -> str:
        """Build node ID for WebSocket path (matches registration ID)."""
        return f"{self._machine_name}-seat-0._ozma._udp.local."

    async def _connect_config_ws(self) -> None:
        """
        Connect to controller WebSocket for seat config push.

        Reconnects with exponential backoff. On each connect the controller
        sends current authoritative config, which we apply if different from
        local state.
        """
        try:
            import aiohttp
        except ImportError:
            log_config.warning("aiohttp not available — config push disabled")
            return

        base = self._controller_url.rstrip("/")
        # Convert http(s) to ws(s)
        ws_base = base.replace("https://", "wss://").replace("http://", "ws://")
        node_id = self._node_id()
        url = f"{ws_base}/api/v1/nodes/{node_id}/config/ws"

        backoff = 1.0
        max_backoff = 30.0

        while not self._stop_event.is_set():
            try:
                async with aiohttp.ClientSession() as session:
                    log_config.info("Connecting to config WS: %s", url)
                    async with session.ws_connect(url, heartbeat=30) as ws:
                        backoff = 1.0  # reset on successful connect
                        log_config.info("Config WS connected")

                        async for msg in ws:
                            if self._stop_event.is_set():
                                break
                            if msg.type == aiohttp.WSMsgType.TEXT:
                                await self._handle_config_message(msg.data)
                            elif msg.type in (aiohttp.WSMsgType.CLOSED,
                                              aiohttp.WSMsgType.ERROR):
                                break

            except asyncio.CancelledError:
                return
            except Exception as e:
                log_config.debug("Config WS error: %s", e)

            if self._stop_event.is_set():
                return
            log_config.info("Config WS reconnecting in %.0fs", backoff)
            try:
                await asyncio.wait_for(self._stop_event.wait(), timeout=backoff)
                return  # stop_event was set
            except asyncio.TimeoutError:
                pass
            backoff = min(backoff * 2, max_backoff)

    async def _handle_config_message(self, raw: str) -> None:
        """Handle a config message from the controller."""
        try:
            msg = json.loads(raw)
        except json.JSONDecodeError:
            log_config.warning("Invalid config message: %s", raw[:200])
            return

        msg_type = msg.get("type", "")

        if msg_type == "seat_config":
            target_seats = msg.get("seats", 1)
            profiles = msg.get("profiles", [])
            log_config.info("Received seat config: seats=%d profiles=%s",
                            target_seats, profiles)
            await self._apply_seat_config(target_seats, profiles)
            self._persist_config({"seats": target_seats, "profiles": profiles})

        elif msg_type == "encoder_hint":
            seat_name = msg.get("seat")
            gaming_gpu = msg.get("gaming_gpu")
            if seat_name and gaming_gpu is not None:
                await self.rebalance_encoders(seat_name, int(gaming_gpu))

        else:
            log_config.debug("Unknown config message type: %s", msg_type)

    async def _apply_seat_config(self, target_seats: int, profiles: list[str]) -> None:
        """
        Scale seats up or down to match target_seats.

        Scaling up creates new seats with virtual displays.
        Scaling down destroys excess seats gracefully (highest index first).
        """
        target_seats = max(1, min(8, target_seats))
        current = len(self._seats)

        if target_seats == current:
            log_config.info("Seat count unchanged at %d", current)
            return

        async with self._scaling_lock:
            if target_seats > current:
                await self._scale_up(current, target_seats, profiles)
            else:
                await self._scale_down(current, target_seats)

    async def _scale_up(self, current: int, target: int, profiles: list[str]) -> None:
        """Create additional seats to reach target count."""
        for i in range(current, target):
            name = f"{self._machine_name}-seat-{i}"
            profile_name = profiles[i] if i < len(profiles) else self._profile.name
            profile = get_profile(profile_name)

            # Create virtual display
            display: DisplayInfo | None = None
            if self._display_backend:
                # Check for unused physical displays first
                used_indices = {s.display_index for s in self._seats}
                for d in self._displays:
                    if d.index not in used_indices:
                        display = d
                        break
                if not display:
                    display = self._display_backend.create_virtual(
                        profile.capture_width, profile.capture_height,
                    )
                    if display:
                        self._displays.append(display)

            # Allocate encoder
            encoder_hints = EncoderHints(
                resolution=(profile.capture_width, profile.capture_height),
                fps=profile.capture_fps,
            )
            encoder_session = (
                self._encoder_allocator.allocate(name, encoder_hints)
                if self._encoder_allocator else None
            )

            seat = Seat(
                name=name,
                seat_index=i,
                display_index=display.index if display else i,
                udp_port=self._base_udp_port + i,
                api_port=self._base_api_port + i,
                capture_fps=profile.capture_fps,
                capture_width=profile.capture_width,
                capture_height=profile.capture_height,
                encoder_args=encoder_session.ffmpeg_args if encoder_session else [],
            )
            seat.display = display

            # Audio sink
            if self._audio_backend:
                sink = await self._audio_backend.create_sink(name)
                seat.audio_sink = sink

            # Set up isolation if profile requests it
            if profile.isolation != "none":
                try:
                    await self._isolation_manager.setup_seat(
                        name, profile.isolation, i,
                    )
                except (ValueError, RuntimeError) as e:
                    log.warning("Seat %s: isolation setup failed (%s)", name, e)

            self._seats.append(seat)

            # Start seat
            task = asyncio.create_task(
                seat.start(self._controller_url),
                name=f"seat-{name}",
            )
            self._seat_tasks.append(task)

            log.info("Scaled up: created seat %s (index=%d)", name, i)

    async def _scale_down(self, current: int, target: int) -> None:
        """Destroy excess seats (highest index first) to reach target count."""
        for i in range(current - 1, target - 1, -1):
            seat = self._seats[i]
            seat_name = seat.name
            log.info("Scaling down: destroying seat %s (index=%d)", seat_name, i)

            # Stop game if running
            if self._game_launcher:
                await self._game_launcher.stop(seat)

            # Tear down isolation
            await self._isolation_manager.teardown_seat(seat_name)

            # Stop the seat
            await seat.stop()

            # Release encoder
            if self._encoder_allocator:
                self._encoder_allocator.release(seat_name)

            # Destroy audio sink
            if self._audio_backend:
                await self._audio_backend.destroy_sink(seat_name)

            # Destroy virtual display
            if seat.display and seat.display.virtual and self._display_backend:
                self._display_backend.destroy_virtual(seat.display)
                if seat.display in self._displays:
                    self._displays.remove(seat.display)

            # Cancel task
            for task in self._seat_tasks:
                if task.get_name() == f"seat-{seat_name}":
                    task.cancel()
                    try:
                        await task
                    except asyncio.CancelledError:
                        pass
                    self._seat_tasks.remove(task)
                    break

            self._seats.pop(i)
            log.info("Scaled down: destroyed seat %s", seat_name)

    # ── Config persistence ───────────────────────────────────────────────────

    def _persist_config(self, config: dict) -> None:
        """Save seat config to disk for offline restarts."""
        try:
            _DEFAULT_CONFIG_DIR.mkdir(parents=True, exist_ok=True)
            with open(_DEFAULT_CONFIG_FILE, "w") as f:
                json.dump(config, f, indent=2)
            log_config.debug("Persisted seat config to %s", _DEFAULT_CONFIG_FILE)
        except Exception as e:
            log_config.warning("Failed to persist seat config: %s", e)

    @staticmethod
    def load_persisted_config() -> dict | None:
        """Load persisted seat config from disk. Returns None if not found."""
        if not _DEFAULT_CONFIG_FILE.exists():
            return None
        try:
            with open(_DEFAULT_CONFIG_FILE) as f:
                config = json.load(f)
            if isinstance(config, dict) and "seats" in config:
                log_config.info("Loaded persisted config: %s", config)
                return config
            return None
        except Exception as e:
            log_config.warning("Failed to load persisted config: %s", e)
            return None

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
        if self._isolation_manager:
            result["isolation"] = self._isolation_manager.to_dict()
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
