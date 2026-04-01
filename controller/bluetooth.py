# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Bluetooth management for ozma.

Wraps BlueZ (bluetoothctl) to manage Bluetooth devices across the ozma
system: pairing, connecting, audio routing, and pair key migration.

Three major capabilities:

1. **Device management** — discover, pair, connect, disconnect BT devices
   from the controller.  Phones for audio (A2DP), controllers for input,
   headsets for voice.

2. **Pair migration** — export pairing keys from one device and import
   them to another.  "Pair once, use everywhere."  Bluetooth link keys
   live in /var/lib/bluetooth/<adapter>/<device>/info on Linux.  Ozma
   can sync these between controller and nodes, so a phone paired to
   the controller can be connected by any node without re-pairing.

3. **Scenario integration** — scenarios can specify which BT devices to
   connect on activation.  Switch to "Gaming" → connect the PS5 controller.
   Switch to "Work" → connect the phone for audio.

Uses bluetoothctl as subprocess for maximum compatibility.  BlueZ D-Bus
API (via dbus-next/dbus-fast) is a future optimisation.

Pair key structure (/var/lib/bluetooth/<adapter_mac>/<device_mac>/info):
  [LinkKey]
  Key=<hex>
  Type=<int>
  PINLength=<int>

  [LongTermKey]        (BLE)
  Key=<hex>
  Authenticated=<bool>
  ...
"""

from __future__ import annotations

import asyncio
import json
import logging
import re
import shutil
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.bluetooth")

BT_PAIR_DIR = Path("/var/lib/bluetooth")


@dataclass
class BTDevice:
    """A discovered or paired Bluetooth device."""

    address: str                    # MAC address (AA:BB:CC:DD:EE:FF)
    name: str = ""
    paired: bool = False
    connected: bool = False
    trusted: bool = False
    device_type: str = "unknown"    # "audio", "controller", "phone", "headset", "keyboard", "mouse", "unknown"
    icon: str = ""                  # BlueZ icon hint
    rssi: int | None = None

    def to_dict(self) -> dict[str, Any]:
        return {
            "address": self.address,
            "name": self.name,
            "paired": self.paired,
            "connected": self.connected,
            "trusted": self.trusted,
            "device_type": self.device_type,
            "rssi": self.rssi,
        }


@dataclass
class PairKey:
    """Exported Bluetooth pairing key for migration."""

    adapter_mac: str                # Controller/node adapter MAC
    device_mac: str                 # Remote device MAC
    device_name: str = ""
    info_contents: str = ""         # Full contents of the info file
    key_type: str = ""              # "LinkKey", "LongTermKey", etc.

    def to_dict(self) -> dict[str, Any]:
        return {
            "adapter_mac": self.adapter_mac,
            "device_mac": self.device_mac,
            "device_name": self.device_name,
            "key_type": self.key_type,
            "has_key": bool(self.info_contents),
        }


class BluetoothManager:
    """
    Manages Bluetooth devices via bluetoothctl subprocess.

    Usage::

        bt = BluetoothManager()
        await bt.start()

        devices = await bt.discover(timeout=10)
        await bt.pair("AA:BB:CC:DD:EE:FF")
        await bt.connect("AA:BB:CC:DD:EE:FF")

        # Export pair key for migration
        key = bt.export_pair_key("AA:BB:CC:DD:EE:FF")

        # On another device: import the key
        bt.import_pair_key(key)
    """

    def __init__(self) -> None:
        self._devices: dict[str, BTDevice] = {}
        self._adapter_mac: str | None = None
        self._available = False
        self._scan_task: asyncio.Task | None = None

    @property
    def available(self) -> bool:
        return self._available

    async def start(self) -> None:
        if not shutil.which("bluetoothctl"):
            log.info("bluetoothctl not found — Bluetooth disabled")
            return

        # Get adapter info
        self._adapter_mac = await self._get_adapter_mac()
        if not self._adapter_mac:
            log.info("No Bluetooth adapter found")
            return

        self._available = True
        await self._load_paired_devices()
        log.info("Bluetooth ready: adapter %s, %d paired device(s)",
                 self._adapter_mac, sum(1 for d in self._devices.values() if d.paired))

    async def stop(self) -> None:
        if self._scan_task:
            self._scan_task.cancel()

    # ── Device discovery and management ──────────────────────────────────────

    async def discover(self, timeout: float = 10.0) -> list[BTDevice]:
        """Run BT discovery scan. Returns list of found devices."""
        if not self._available:
            return []

        # Start scanning
        await self._btctl("scan", "on")
        await asyncio.sleep(timeout)
        await self._btctl("scan", "off")

        # Parse discovered devices
        output = await self._btctl("devices")
        if output:
            for line in output.splitlines():
                m = re.match(r"Device\s+([0-9A-Fa-f:]{17})\s+(.*)", line.strip())
                if m:
                    addr, name = m.group(1), m.group(2)
                    if addr not in self._devices:
                        self._devices[addr] = BTDevice(address=addr, name=name)
                    else:
                        self._devices[addr].name = name

        return list(self._devices.values())

    async def pair(self, address: str) -> bool:
        """Pair with a device."""
        if not self._available:
            return False
        output = await self._btctl("pair", address, timeout=30)
        if output and "Pairing successful" in output:
            await self._btctl("trust", address)
            if address in self._devices:
                self._devices[address].paired = True
                self._devices[address].trusted = True
            else:
                self._devices[address] = BTDevice(
                    address=address, paired=True, trusted=True,
                )
            log.info("Paired: %s", address)
            return True
        log.warning("Pair failed: %s — %s", address, output)
        return False

    async def connect(self, address: str) -> bool:
        """Connect to a paired device."""
        if not self._available:
            return False
        output = await self._btctl("connect", address, timeout=10)
        if output and "Connection successful" in output:
            if address in self._devices:
                self._devices[address].connected = True
            log.info("Connected: %s", address)
            return True
        log.warning("Connect failed: %s — %s", address, output)
        return False

    async def disconnect(self, address: str) -> bool:
        """Disconnect a device."""
        if not self._available:
            return False
        output = await self._btctl("disconnect", address)
        if address in self._devices:
            self._devices[address].connected = False
        return True

    async def remove(self, address: str) -> bool:
        """Remove (unpair) a device."""
        if not self._available:
            return False
        await self._btctl("remove", address)
        self._devices.pop(address, None)
        return True

    def list_devices(self) -> list[dict[str, Any]]:
        return [d.to_dict() for d in self._devices.values()]

    def get_device(self, address: str) -> BTDevice | None:
        return self._devices.get(address)

    # ── Pair key export/import (migration) ───────────────────────────────────

    def export_pair_key(self, device_mac: str) -> PairKey | None:
        """
        Export the pairing key for a device.

        Reads the info file from /var/lib/bluetooth/<adapter>/<device>/info.
        This contains the link key needed to establish an encrypted connection
        without re-pairing.
        """
        if not self._adapter_mac:
            return None

        adapter_dir = BT_PAIR_DIR / self._adapter_mac.upper().replace(":", "")
        # BlueZ stores as AA:BB:CC:DD:EE:FF directory name
        device_dir = adapter_dir / device_mac.upper()
        if not device_dir.exists():
            # Try with colons replaced
            for d in adapter_dir.iterdir():
                if d.name.upper().replace(":", "") == device_mac.upper().replace(":", ""):
                    device_dir = d
                    break

        info_file = device_dir / "info"
        if not info_file.exists():
            log.debug("No pair key found for %s", device_mac)
            return None

        contents = info_file.read_text()

        # Detect key type
        key_type = ""
        if "[LinkKey]" in contents:
            key_type = "LinkKey"
        elif "[LongTermKey]" in contents:
            key_type = "LongTermKey"

        device = self._devices.get(device_mac)
        return PairKey(
            adapter_mac=self._adapter_mac,
            device_mac=device_mac,
            device_name=device.name if device else "",
            info_contents=contents,
            key_type=key_type,
        )

    def import_pair_key(self, pair_key: PairKey, target_adapter_mac: str | None = None) -> bool:
        """
        Import a pairing key (from another device).

        Writes the info file to /var/lib/bluetooth/<adapter>/<device>/info.
        After import, the device will appear as paired and can be connected
        without going through the pairing process again.

        Requires root access (or appropriate file permissions).
        """
        adapter = target_adapter_mac or self._adapter_mac
        if not adapter:
            return False

        device_dir = BT_PAIR_DIR / adapter.upper() / pair_key.device_mac.upper()
        try:
            device_dir.mkdir(parents=True, exist_ok=True)
            (device_dir / "info").write_text(pair_key.info_contents)
            log.info("Imported pair key: %s → adapter %s", pair_key.device_mac, adapter)
            return True
        except PermissionError:
            log.warning("Cannot import pair key — need root access to %s", device_dir)
            return False
        except Exception as e:
            log.warning("Import pair key failed: %s", e)
            return False

    def export_all_pair_keys(self) -> list[PairKey]:
        """Export all pairing keys from the local adapter."""
        keys = []
        for device in self._devices.values():
            if device.paired:
                key = self.export_pair_key(device.address)
                if key:
                    keys.append(key)
        return keys

    # ── Scenario integration ─────────────────────────────────────────────────

    async def on_scenario_switch(self, bt_config: dict | None) -> None:
        """
        Apply scenario Bluetooth config.

        bt_config format:
          {"connect": ["AA:BB:CC:DD:EE:FF"], "disconnect": ["XX:XX:XX:XX:XX:XX"]}
        """
        if not bt_config or not self._available:
            return

        for addr in bt_config.get("disconnect", []):
            await self.disconnect(addr)

        for addr in bt_config.get("connect", []):
            await self.connect(addr)

    # ── Internals ────────────────────────────────────────────────────────────

    async def _get_adapter_mac(self) -> str | None:
        """Get the default BT adapter MAC address."""
        output = await self._btctl("list")
        if output:
            m = re.search(r"Controller\s+([0-9A-Fa-f:]{17})", output)
            if m:
                return m.group(1)
        return None

    async def _load_paired_devices(self) -> None:
        """Load currently paired devices from bluetoothctl."""
        output = await self._btctl("devices", "Paired")
        if not output:
            # Fallback: list all devices and check paired status
            output = await self._btctl("devices")
        if not output:
            return

        for line in output.splitlines():
            m = re.match(r"Device\s+([0-9A-Fa-f:]{17})\s+(.*)", line.strip())
            if m:
                addr, name = m.group(1), m.group(2)
                device = BTDevice(address=addr, name=name, paired=True, trusted=True)
                device.device_type = self._guess_device_type(name)
                self._devices[addr] = device

    @staticmethod
    def _guess_device_type(name: str) -> str:
        """Best-effort device type from name."""
        name_lower = name.lower()
        if any(k in name_lower for k in ("airpods", "buds", "headphone", "headset", "jabra", "wh-", "wf-")):
            return "headset"
        if any(k in name_lower for k in ("iphone", "galaxy", "pixel", "phone")):
            return "phone"
        if any(k in name_lower for k in ("controller", "dualsense", "dualshock", "xbox", "joycon", "pro controller")):
            return "controller"
        if any(k in name_lower for k in ("keyboard", "keychron", "k380")):
            return "keyboard"
        if any(k in name_lower for k in ("mouse", "mx master", "trackpad")):
            return "mouse"
        if any(k in name_lower for k in ("speaker", "sonos", "jbl", "bose", "marshall", "echo")):
            return "audio"
        return "unknown"

    async def _btctl(self, *args: str, timeout: float = 10.0) -> str | None:
        """Run a bluetoothctl command and return stdout."""
        try:
            proc = await asyncio.create_subprocess_exec(
                "bluetoothctl", *args,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
                stdin=asyncio.subprocess.DEVNULL,
            )
            stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=timeout)
            return stdout.decode(errors="replace").strip()
        except asyncio.TimeoutError:
            log.debug("bluetoothctl %s timed out", " ".join(args))
            return None
        except FileNotFoundError:
            return None
        except Exception as e:
            log.debug("bluetoothctl error: %s", e)
            return None
