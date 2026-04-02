#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Ozma evdev device service — persistent virtual input devices for VMs.

Creates and maintains uinput keyboard/mouse devices that QEMU reads via
input-linux. Runs as a systemd service — devices survive soft node
restarts since this process owns the uinput file descriptors.

The devices are created on-demand via a Unix socket API. The VNM calls
this service to create devices for each VM, then adds input-linux
objects to the VM's XML.

Each VM gets:
  /dev/input/by-id/ozma-kbd-<vm>   → virtual keyboard
  /dev/input/by-id/ozma-mouse-<vm> → virtual mouse

The soft node writes HID events to these devices via the evdev_input
module. QEMU reads them via input-linux — zero latency, no QMP.

Usage:
  systemctl start ozma-evdev    # as a service
  python3 evdev_service.py      # standalone
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import signal
import sys
from pathlib import Path

log = logging.getLogger("ozma.evdev_service")

sys.path.insert(0, str(Path(__file__).parent))
from evdev_input import VirtualKeyboard, VirtualMouse

SOCKET_PATH = "/run/ozma/evdev.sock"
STATE_DIR = "/run/ozma/evdev"


class EvdevService:
    """Manages persistent virtual evdev devices for VMs."""

    def __init__(self):
        self._devices: dict[str, tuple[VirtualKeyboard, VirtualMouse]] = {}
        self._stop = asyncio.Event()

    def create(self, vm_name: str) -> dict:
        """Create keyboard + mouse for a VM. Returns device paths."""
        if vm_name in self._devices:
            kbd, mouse = self._devices[vm_name]
            return {"kbd": kbd.path, "mouse": mouse.path, "existed": True}

        kbd = VirtualKeyboard(name=f"ozma-kbd-{vm_name}")
        mouse = VirtualMouse(name=f"ozma-mouse-{vm_name}")

        kbd_path = kbd.start()
        mouse_path = mouse.start()

        if not kbd_path or not mouse_path:
            kbd.stop()
            mouse.stop()
            return {"error": f"Failed to create devices for {vm_name}"}

        self._devices[vm_name] = (kbd, mouse)

        # Write state files
        os.makedirs(STATE_DIR, exist_ok=True)
        Path(f"{STATE_DIR}/{vm_name}.kbd").write_text(kbd_path)
        Path(f"{STATE_DIR}/{vm_name}.mouse").write_text(mouse_path)

        log.info("Created devices for %s: kbd=%s mouse=%s", vm_name, kbd_path, mouse_path)
        return {"kbd": kbd_path, "mouse": mouse_path, "existed": False}

    def destroy(self, vm_name: str) -> dict:
        """Destroy devices for a VM."""
        devices = self._devices.pop(vm_name, None)
        if devices:
            kbd, mouse = devices
            kbd.stop()
            mouse.stop()
            for f in [f"{STATE_DIR}/{vm_name}.kbd", f"{STATE_DIR}/{vm_name}.mouse"]:
                try:
                    os.unlink(f)
                except FileNotFoundError:
                    pass
            log.info("Destroyed devices for %s", vm_name)
            return {"ok": True}
        return {"error": "No devices for this VM"}

    def list_devices(self) -> dict:
        """List all managed devices."""
        return {
            name: {"kbd": kbd.path, "mouse": mouse.path}
            for name, (kbd, mouse) in self._devices.items()
        }

    def get_paths(self, vm_name: str) -> tuple[str, str] | None:
        """Get (kbd_path, mouse_path) for a VM, or None."""
        devices = self._devices.get(vm_name)
        if devices:
            return (devices[0].path, devices[1].path)
        return None

    async def run(self):
        """Run the service with a Unix socket API."""
        os.makedirs(os.path.dirname(SOCKET_PATH), exist_ok=True)
        try:
            os.unlink(SOCKET_PATH)
        except FileNotFoundError:
            pass

        server = await asyncio.start_unix_server(self._handle_client, SOCKET_PATH)
        os.chmod(SOCKET_PATH, 0o777)  # anyone can request device creation
        log.info("Evdev service listening on %s", SOCKET_PATH)

        await self._stop.wait()

        server.close()
        # Cleanup all devices
        for name in list(self._devices):
            self.destroy(name)

    async def _handle_client(self, reader: asyncio.StreamReader,
                             writer: asyncio.StreamWriter):
        try:
            data = await asyncio.wait_for(reader.readline(), timeout=5)
            request = json.loads(data)
            action = request.get("action", "")

            if action == "create":
                result = self.create(request["vm_name"])
            elif action == "destroy":
                result = self.destroy(request["vm_name"])
            elif action == "list":
                result = self.list_devices()
            elif action == "get":
                paths = self.get_paths(request["vm_name"])
                result = {"kbd": paths[0], "mouse": paths[1]} if paths else {"error": "not found"}
            else:
                result = {"error": f"Unknown action: {action}"}

            writer.write(json.dumps(result).encode() + b"\n")
            await writer.drain()
        except Exception as e:
            try:
                writer.write(json.dumps({"error": str(e)}).encode() + b"\n")
                await writer.drain()
            except Exception:
                pass
        finally:
            writer.close()


async def _request(action: str, **kwargs) -> dict:
    """Send a request to the running evdev service."""
    reader, writer = await asyncio.open_unix_connection(SOCKET_PATH)
    writer.write(json.dumps({"action": action, **kwargs}).encode() + b"\n")
    await writer.drain()
    resp = await reader.readline()
    writer.close()
    return json.loads(resp)


async def create_devices(vm_name: str) -> dict:
    """Request device creation from the service."""
    return await _request("create", vm_name=vm_name)


async def get_device_paths(vm_name: str) -> tuple[str, str] | None:
    """Get device paths from the service."""
    result = await _request("get", vm_name=vm_name)
    if "error" in result:
        return None
    return (result["kbd"], result["mouse"])


def main():
    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
        datefmt="%H:%M:%S",
    )

    service = EvdevService()
    loop = asyncio.new_event_loop()

    def _stop():
        service._stop.set()

    for sig in (signal.SIGINT, signal.SIGTERM):
        loop.add_signal_handler(sig, _stop)

    try:
        loop.run_until_complete(service.run())
    except KeyboardInterrupt:
        pass
    finally:
        loop.close()


if __name__ == "__main__":
    main()
