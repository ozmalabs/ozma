# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
USB hotplug auto-detection for multi-seat.

Monitors for USB device plug/unplug events and auto-creates/destroys seats.
Linux uses pyudev for real-time monitoring; Windows polls GetRawInputDeviceList.

Seat persistence: device-to-seat mappings are saved to disk so that the
same USB devices restore to the same seats across reboots.

Usage:
    hotplug = HotplugMonitor(seat_manager)
    await hotplug.start()
    ...
    await hotplug.stop()
"""

from __future__ import annotations

import asyncio
import json
import logging
import platform
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import TYPE_CHECKING, Any

from .input_router import InputGroup
from .usb_topology import USBTopologyScanner

if TYPE_CHECKING:
    from .seat import Seat
    from .seat_manager import SeatManager

log = logging.getLogger("ozma.agent.multiseat.hotplug")

# Debounce: wait for all devices in a USB group to finish enumerating
SETTLE_DELAY_S = 0.5

# Grace period: don't destroy a seat immediately when devices disappear
# (USB glitch, hub reset, etc.)
REMOVAL_GRACE_S = 5.0

# Windows polling interval
POLL_INTERVAL_S = 2.0


# ── Seat persistence ─────────────────────────────────────────────────────────

def _persistence_path() -> Path:
    """Return the path for the seat persistence file."""
    system = platform.system()
    if system == "Windows":
        appdata = Path(os.environ.get("APPDATA", Path.home() / "AppData" / "Roaming"))
        return appdata / "ozma" / "seats.json"
    else:
        config_dir = Path(os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config"))
        return config_dir / "ozma" / "seats.json"


import os


@dataclass
class SeatMapping:
    """Persisted mapping from USB hub path to seat configuration."""
    hub_path: str
    seat_name: str
    seat_index: int
    device_signatures: list[str] = field(default_factory=list)  # VID:PID strings

    def to_dict(self) -> dict:
        return {
            "hub_path": self.hub_path,
            "seat_name": self.seat_name,
            "seat_index": self.seat_index,
            "device_signatures": self.device_signatures,
        }

    @staticmethod
    def from_dict(data: dict) -> SeatMapping:
        return SeatMapping(
            hub_path=data.get("hub_path", ""),
            seat_name=data.get("seat_name", ""),
            seat_index=data.get("seat_index", 0),
            device_signatures=data.get("device_signatures", []),
        )


class SeatPersistence:
    """Load and save seat-to-device mappings for reboot persistence."""

    def __init__(self, path: Path | None = None) -> None:
        self._path = path or _persistence_path()
        self._mappings: dict[str, SeatMapping] = {}

    def load(self) -> dict[str, SeatMapping]:
        """Load mappings from disk. Returns empty dict on failure."""
        if not self._path.exists():
            return {}

        try:
            text = self._path.read_text(errors="replace")
            data = json.loads(text)
            if not isinstance(data, list):
                log.warning("Seat persistence: expected list, got %s", type(data).__name__)
                return {}

            self._mappings = {}
            for entry in data:
                if isinstance(entry, dict) and entry.get("hub_path"):
                    mapping = SeatMapping.from_dict(entry)
                    self._mappings[mapping.hub_path] = mapping

            log.info("Loaded %d seat mappings from %s", len(self._mappings), self._path)
            return dict(self._mappings)

        except json.JSONDecodeError as e:
            log.warning("Seat persistence: corrupt JSON in %s: %s", self._path, e)
            return {}
        except Exception as e:
            log.warning("Seat persistence: failed to load %s: %s", self._path, e)
            return {}

    def save(self) -> bool:
        """Save current mappings to disk."""
        try:
            self._path.parent.mkdir(parents=True, exist_ok=True)
            data = [m.to_dict() for m in self._mappings.values()]
            text = json.dumps(data, indent=2)
            self._path.write_text(text)
            log.debug("Saved %d seat mappings to %s", len(self._mappings), self._path)
            return True
        except Exception as e:
            log.warning("Seat persistence: failed to save %s: %s", self._path, e)
            return False

    def get(self, hub_path: str) -> SeatMapping | None:
        return self._mappings.get(hub_path)

    def set(self, mapping: SeatMapping) -> None:
        self._mappings[mapping.hub_path] = mapping

    def remove(self, hub_path: str) -> None:
        self._mappings.pop(hub_path, None)

    @property
    def mappings(self) -> dict[str, SeatMapping]:
        return dict(self._mappings)


# ── Pending device state tracking ────────────────────────────────────────────

@dataclass
class _PendingAddition:
    """Tracks a USB group that is settling after device plug-in."""
    hub_path: str
    first_seen: float
    group: InputGroup | None = None


@dataclass
class _PendingRemoval:
    """Tracks a seat whose devices disappeared (grace period before destroy)."""
    seat_name: str
    removed_at: float
    cancel_event: asyncio.Event = field(default_factory=asyncio.Event)


# ── Hotplug Monitor ──────────────────────────────────────────────────────────

class HotplugMonitor:
    """
    Monitor USB device plug/unplug events and auto-manage seats.

    On Linux: uses pyudev for real-time udev event monitoring.
    On Windows: polls GetRawInputDeviceList every 2 seconds.

    When a keyboard+mouse pair appears on a new USB hub:
      - Wait 500ms for all devices to settle (debounce)
      - Run USB topology scan
      - If the group has keyboard+mouse and is unassigned: create a seat
      - Restore seat name from persistence if the same hub was used before

    When devices disappear:
      - Mark the seat as "input lost"
      - After 5s grace period: destroy the seat
      - If devices return within 5s: cancel destruction
    """

    def __init__(self, seat_manager: SeatManager) -> None:
        self._seat_manager = seat_manager
        self._scanner = USBTopologyScanner()
        self._persistence = SeatPersistence()
        self._stop_event = asyncio.Event()
        self._monitor_task: asyncio.Task | None = None

        # Current state
        self._known_groups: dict[str, InputGroup] = {}  # hub_path -> group
        self._hub_to_seat: dict[str, str] = {}  # hub_path -> seat_name
        self._pending_additions: dict[str, _PendingAddition] = {}
        self._pending_removals: dict[str, _PendingRemoval] = {}

    @property
    def persistence(self) -> SeatPersistence:
        return self._persistence

    async def start(self) -> None:
        """Start the hotplug monitor."""
        # Load persistent mappings
        self._persistence.load()

        # Initial scan to build baseline
        groups = await asyncio.get_running_loop().run_in_executor(
            None, self._scanner.scan,
        )
        for group in groups:
            self._known_groups[group.hub_path] = group

        log.info("Hotplug monitor starting: %d known input groups", len(self._known_groups))

        system = platform.system()
        if system == "Linux":
            self._monitor_task = asyncio.create_task(
                self._monitor_linux(), name="hotplug-linux",
            )
        else:
            # Windows or unsupported: poll-based
            self._monitor_task = asyncio.create_task(
                self._monitor_poll(), name="hotplug-poll",
            )

    async def stop(self) -> None:
        """Stop the hotplug monitor."""
        self._stop_event.set()

        # Cancel any pending removals
        for pending in self._pending_removals.values():
            pending.cancel_event.set()
        self._pending_removals.clear()

        if self._monitor_task:
            self._monitor_task.cancel()
            try:
                await self._monitor_task
            except asyncio.CancelledError:
                pass
            self._monitor_task = None

        # Save persistence
        self._persistence.save()
        log.info("Hotplug monitor stopped")

    # ── Linux: pyudev monitoring ─────────────────────────────────────────────

    async def _monitor_linux(self) -> None:
        """Monitor udev events for input device changes (Linux)."""
        try:
            import pyudev
        except ImportError:
            log.warning("pyudev not available — falling back to polling")
            await self._monitor_poll()
            return

        loop = asyncio.get_running_loop()
        context = pyudev.Context()
        monitor = pyudev.Monitor.from_netlink(context)
        monitor.filter_by(subsystem="input")
        monitor.start()

        # Make the monitor non-blocking so we can poll it in asyncio
        fileno = monitor.fileno()
        settle_timers: dict[str, asyncio.TimerHandle] = {}

        def _on_readable() -> None:
            """Called when udev monitor has data ready."""
            device = monitor.poll(timeout=0)
            if not device:
                return

            action = device.action
            if action not in ("add", "remove"):
                return

            # Only care about eventN devices
            dev_node = device.device_node
            if not dev_node or not dev_node.startswith("/dev/input/event"):
                return

            log.debug("udev: %s %s", action, dev_node)

            # Cancel any existing settle timer for this action type
            # and schedule a new rescan after the settle delay
            timer_key = f"{action}"
            existing = settle_timers.pop(timer_key, None)
            if existing:
                existing.cancel()

            settle_timers[timer_key] = loop.call_later(
                SETTLE_DELAY_S,
                lambda a=action: asyncio.create_task(
                    self._on_devices_changed(a),
                    name=f"hotplug-{a}",
                ),
            )

        loop.add_reader(fileno, _on_readable)

        try:
            await self._stop_event.wait()
        finally:
            loop.remove_reader(fileno)
            for timer in settle_timers.values():
                timer.cancel()

    # ── Poll-based monitoring (Windows / fallback) ───────────────────────────

    async def _monitor_poll(self) -> None:
        """Poll-based hotplug monitoring (Windows or pyudev fallback)."""
        while not self._stop_event.is_set():
            try:
                await asyncio.wait_for(
                    self._stop_event.wait(),
                    timeout=POLL_INTERVAL_S,
                )
                break  # stop event was set
            except asyncio.TimeoutError:
                pass  # timeout = time to poll

            await self._on_devices_changed("poll")

    # ── Device change handling ───────────────────────────────────────────────

    async def _on_devices_changed(self, action: str) -> None:
        """Handle a batch of device changes after the settle delay."""
        # Rescan USB topology
        loop = asyncio.get_running_loop()
        new_groups = await loop.run_in_executor(None, self._scanner.scan)

        new_by_hub: dict[str, InputGroup] = {g.hub_path: g for g in new_groups}

        # Find added groups (new hub paths with keyboard+mouse)
        for hub_path, group in new_by_hub.items():
            if hub_path not in self._known_groups and group.has_input:
                if hub_path not in self._hub_to_seat:
                    log.info("New input group detected: hub=%s (kbd=%d, mice=%d)",
                             hub_path, len(group.keyboards), len(group.mice))
                    await self._on_group_added(group)
                elif hub_path in self._pending_removals:
                    # Device came back within grace period — cancel removal
                    pending = self._pending_removals.pop(hub_path)
                    pending.cancel_event.set()
                    log.info("Input group %s returned within grace period — "
                             "cancelling seat destruction", hub_path)

        # Find removed groups (hub paths that disappeared)
        for hub_path, group in self._known_groups.items():
            if hub_path not in new_by_hub:
                if hub_path in self._hub_to_seat and hub_path not in self._pending_removals:
                    log.info("Input group removed: hub=%s", hub_path)
                    await self._on_group_removed(hub_path)

        # Also check groups that lost their keyboard+mouse
        for hub_path, group in new_by_hub.items():
            if hub_path in self._known_groups and hub_path in self._hub_to_seat:
                if not group.has_input and hub_path not in self._pending_removals:
                    log.info("Input group lost keyboard/mouse: hub=%s", hub_path)
                    await self._on_group_removed(hub_path)

        # Update known state
        self._known_groups = new_by_hub

    async def _on_group_added(self, group: InputGroup) -> None:
        """Handle a new input group (keyboard+mouse plugged in)."""
        hub_path = group.hub_path

        # Check persistence for a previous seat mapping
        persisted = self._persistence.get(hub_path)
        if persisted:
            seat_name = persisted.seat_name
            seat_index = persisted.seat_index
            log.info("Restoring seat %s (index=%d) for hub %s",
                     seat_name, seat_index, hub_path)
        else:
            # Assign next available index
            used_indices = set()
            for seat in self._seat_manager.seats:
                used_indices.add(seat.seat_index)
            seat_index = 0
            while seat_index in used_indices:
                seat_index += 1

            machine_name = self._seat_manager._machine_name
            seat_name = f"{machine_name}-seat-{seat_index}"

        # Create the seat via the seat manager
        seat = await self._seat_manager.create_hotplug_seat(
            name=seat_name,
            seat_index=seat_index,
            input_group=group,
        )

        if seat:
            self._hub_to_seat[hub_path] = seat.name

            # Get device signatures for persistence
            signatures = self._get_device_signatures(group)

            # Save persistence
            self._persistence.set(SeatMapping(
                hub_path=hub_path,
                seat_name=seat.name,
                seat_index=seat.seat_index,
                device_signatures=signatures,
            ))
            self._persistence.save()

            log.info("Hotplug: created seat %s for hub %s", seat.name, hub_path)
        else:
            log.warning("Hotplug: failed to create seat for hub %s", hub_path)

    async def _on_group_removed(self, hub_path: str) -> None:
        """Handle input group removal — start grace period before destroying seat."""
        seat_name = self._hub_to_seat.get(hub_path)
        if not seat_name:
            return

        pending = _PendingRemoval(
            seat_name=seat_name,
            removed_at=time.time(),
        )
        self._pending_removals[hub_path] = pending

        log.info("Input lost on seat %s (hub=%s) — %ds grace period",
                 seat_name, hub_path, int(REMOVAL_GRACE_S))

        asyncio.create_task(
            self._grace_period_destroy(hub_path, pending),
            name=f"hotplug-grace-{hub_path}",
        )

    async def _grace_period_destroy(self, hub_path: str, pending: _PendingRemoval) -> None:
        """Wait out the grace period, then destroy the seat if still gone."""
        try:
            await asyncio.wait_for(pending.cancel_event.wait(), timeout=REMOVAL_GRACE_S)
            # Cancel event was set — device came back
            log.debug("Grace period cancelled for seat %s", pending.seat_name)
            return
        except asyncio.TimeoutError:
            pass  # Grace period expired — destroy the seat

        # Clean up
        self._pending_removals.pop(hub_path, None)
        self._hub_to_seat.pop(hub_path, None)

        # Destroy the seat
        await self._seat_manager.destroy_hotplug_seat(pending.seat_name)
        log.info("Hotplug: destroyed seat %s (hub %s removed)", pending.seat_name, hub_path)

    def _get_device_signatures(self, group: InputGroup) -> list[str]:
        """
        Get VID:PID signatures for devices in a group.

        Reads from sysfs on Linux. Returns empty list on failure.
        """
        signatures: list[str] = []

        if platform.system() != "Linux":
            return signatures

        for dev_path in group.all_devices:
            event_name = Path(dev_path).name
            # Try to read vendor/product from sysfs
            device_dir = Path(f"/sys/class/input/{event_name}/device")
            try:
                real = device_dir.resolve()
                # Walk up to find USB device with idVendor/idProduct
                parts = list(real.parents)
                for parent in [real] + parts:
                    vid_path = parent / "idVendor"
                    pid_path = parent / "idProduct"
                    if vid_path.exists() and pid_path.exists():
                        vid = vid_path.read_text().strip()
                        pid = pid_path.read_text().strip()
                        sig = f"{vid}:{pid}"
                        if sig not in signatures:
                            signatures.append(sig)
                        break
            except Exception:
                continue

        return signatures

    def to_dict(self) -> dict:
        """Serialize hotplug state for diagnostics."""
        return {
            "known_groups": len(self._known_groups),
            "hub_to_seat": dict(self._hub_to_seat),
            "pending_additions": len(self._pending_additions),
            "pending_removals": {
                hub: {"seat": p.seat_name, "removed_at": p.removed_at}
                for hub, p in self._pending_removals.items()
            },
            "persisted_mappings": len(self._persistence.mappings),
        }
