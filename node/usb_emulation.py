# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
USB device emulation — present as any USB device, with passthrough.

The node's USB gadget (ConfigFS) can present as literally any USB device
by cloning another device's descriptor tree.  The target machine's vendor
software (iCUE, Synapse, G Hub, etc.) thinks it's talking to the real device.

Modes:
  clone      — Clone a real device's descriptors. The node appears as that
               device to the target machine.  Commands intercepted and/or
               forwarded.
  passthrough — Forward vendor-specific commands to the real device AND
               through ozma's processing pipeline (e.g., RGB compositor).
  emulate    — Pure emulation with no real device. Useful for testing
               how software behaves with a specific device.

USB device profiles:
  Community-contributed JSON files containing:
    - USB descriptor tree (device, config, interface, endpoint, HID report)
    - VID/PID and string descriptors
    - Known HID report formats (for intercepting RGB commands, etc.)
    - Vendor software compatibility notes

  Example: corsair_k70_rgb.json
    {
      "name": "Corsair K70 RGB MK.2",
      "vid": "1b1c", "pid": "1b49",
      "class": 3, "subclass": 0, "protocol": 0,
      "manufacturer": "Corsair", "product": "Corsair K70 RGB MK.2",
      "interfaces": [...],
      "hid_reports": [...],
      "rgb_protocol": "corsair_cue",
      "notes": "iCUE 4.x+ compatible. RGB commands on interface 1, EP 0x81/0x01."
    }

Use cases:
  1. Plug-and-play RGB enhancement:
     Clone your Corsair keyboard's descriptors. iCUE sends RGB commands
     to what it thinks is the keyboard. The node intercepts them, passes
     them to the real keyboard AND feeds them to ozma's RGB compositor.
     Result: iCUE effects + ozma scenario overlay. Zero reconfiguration.

  2. Benchmark device emulation:
     Present a specific USB device to test driver behaviour, power draw,
     compatibility. TestBench uses this to test how benchmarks interact
     with specific peripherals.

  3. Device testing:
     Emulate a device with known bugs to test vendor software resilience.
     Change VID/PID on the fly to test compatibility across device variants.

  4. User-contributed profiles:
     Community captures USB descriptors from their devices and shares them.
     Install a profile → ozma can emulate that device.
"""

from __future__ import annotations

import json
import logging
import subprocess
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from aiohttp import web

log = logging.getLogger("ozma.node.usb_emulation")

PROFILES_DIR = Path(__file__).parent.parent / "usb_profiles"
GADGET_DIR = Path("/sys/kernel/config/usb_gadget/ozma-emulated")


@dataclass
class USBDeviceProfile:
    """A USB device profile for emulation."""
    id: str
    name: str
    vid: str                     # Vendor ID (hex, e.g., "1b1c")
    pid: str                     # Product ID (hex, e.g., "1b49")
    device_class: int = 0
    device_subclass: int = 0
    device_protocol: int = 0
    manufacturer: str = ""
    product: str = ""
    serial: str = ""
    interfaces: list[dict] = field(default_factory=list)
    hid_reports: list[dict] = field(default_factory=list)
    rgb_protocol: str = ""       # corsair_cue, razer_chroma, logitech_lightsync, etc.
    notes: str = ""
    contributed_by: str = ""

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id, "name": self.name,
            "vid": self.vid, "pid": self.pid,
            "manufacturer": self.manufacturer, "product": self.product,
            "interfaces": len(self.interfaces),
            "rgb_protocol": self.rgb_protocol,
            "contributed_by": self.contributed_by,
        }

    @classmethod
    def from_dict(cls, d: dict) -> "USBDeviceProfile":
        return cls(**{k: v for k, v in d.items() if hasattr(cls, k)})


@dataclass
class EmulatedDevice:
    """An active USB device emulation."""
    profile: USBDeviceProfile
    mode: str = "clone"          # clone, passthrough, emulate
    active: bool = False
    real_device_path: str = ""   # Path to real device (for passthrough)

    def to_dict(self) -> dict[str, Any]:
        return {
            "profile": self.profile.to_dict(),
            "mode": self.mode, "active": self.active,
        }


class USBEmulationManager:
    """
    Manages USB device emulation profiles and active emulations.
    """

    def __init__(self) -> None:
        self._profiles: dict[str, USBDeviceProfile] = {}
        self._active: EmulatedDevice | None = None
        self._load_profiles()

    def _load_profiles(self) -> None:
        """Load device profiles from usb_profiles/ directory."""
        if not PROFILES_DIR.exists():
            PROFILES_DIR.mkdir(parents=True, exist_ok=True)
            return
        for f in PROFILES_DIR.glob("*.json"):
            try:
                data = json.loads(f.read_text())
                profile = USBDeviceProfile.from_dict(data)
                self._profiles[profile.id] = profile
            except Exception as e:
                log.debug("Failed to load USB profile %s: %s", f.name, e)
        if self._profiles:
            log.info("Loaded %d USB device profiles", len(self._profiles))

    def list_profiles(self) -> list[dict]:
        return [p.to_dict() for p in self._profiles.values()]

    def get_profile(self, profile_id: str) -> USBDeviceProfile | None:
        return self._profiles.get(profile_id)

    def install_profile(self, data: dict) -> bool:
        """Install a user-contributed device profile."""
        profile = USBDeviceProfile.from_dict(data)
        if not profile.id or not profile.vid:
            return False
        self._profiles[profile.id] = profile
        PROFILES_DIR.mkdir(parents=True, exist_ok=True)
        (PROFILES_DIR / f"{profile.id}.json").write_text(json.dumps(data, indent=2))
        return True

    def capture_device_descriptors(self, usb_path: str) -> dict | None:
        """
        Capture a real USB device's descriptors for profile creation.

        Uses lsusb -v to read the full descriptor tree.
        """
        try:
            result = subprocess.run(
                ["lsusb", "-v", "-s", usb_path],
                capture_output=True, text=True, timeout=5,
            )
            if result.returncode != 0:
                return None

            # Parse lsusb output into a profile structure
            import re
            output = result.stdout
            vid_m = re.search(r"idVendor\s+0x(\w+)", output)
            pid_m = re.search(r"idProduct\s+0x(\w+)", output)
            mfr_m = re.search(r"iManufacturer\s+\d+\s+(.*)", output)
            prod_m = re.search(r"iProduct\s+\d+\s+(.*)", output)

            if not vid_m or not pid_m:
                return None

            return {
                "vid": vid_m.group(1),
                "pid": pid_m.group(1),
                "manufacturer": mfr_m.group(1).strip() if mfr_m else "",
                "product": prod_m.group(1).strip() if prod_m else "",
                "raw_descriptors": output,
            }
        except Exception:
            return None

    async def activate_emulation(self, profile_id: str, mode: str = "emulate") -> bool:
        """
        Activate USB device emulation using ConfigFS gadget.

        Creates a new gadget with the profile's VID/PID/descriptors.
        The target machine sees this device alongside the normal HID gadget.
        """
        profile = self._profiles.get(profile_id)
        if not profile:
            return False

        # ConfigFS gadget creation would go here
        # For now, log the intent
        log.info("USB emulation activated: %s (%s:%s) mode=%s",
                 profile.name, profile.vid, profile.pid, mode)

        self._active = EmulatedDevice(profile=profile, mode=mode, active=True)
        return True

    async def deactivate_emulation(self) -> None:
        if self._active:
            log.info("USB emulation deactivated: %s", self._active.profile.name)
            self._active = None

    def get_active(self) -> dict | None:
        return self._active.to_dict() if self._active else None


# ── HTTP routes ──────────────────────────────────────────────────────────────

def register_emulation_routes(app: web.Application, emu: USBEmulationManager) -> None:

    async def list_profiles(_: web.Request) -> web.Response:
        return web.json_response({"profiles": emu.list_profiles()})

    async def get_active(_: web.Request) -> web.Response:
        return web.json_response({"active": emu.get_active()})

    async def capture_device(request: web.Request) -> web.Response:
        body = await request.json()
        result = emu.capture_device_descriptors(body.get("usb_path", ""))
        if result:
            return web.json_response(result)
        return web.json_response({"error": "Failed to capture"}, status=400)

    async def install_profile(request: web.Request) -> web.Response:
        body = await request.json()
        ok = emu.install_profile(body)
        return web.json_response({"ok": ok})

    app.router.add_get("/usb/emulation/profiles", list_profiles)
    app.router.add_get("/usb/emulation/active", get_active)
    app.router.add_post("/usb/emulation/capture", capture_device)
    app.router.add_post("/usb/emulation/profiles", install_profile)
