#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Create ozma evdev input devices for a VM and write marker files.

Called by the libvirt qemu hook (start/begin) to ensure evdev devices
exist before QEMU starts, so input-linux objects work without hot-attach.

Also usable as a CLI tool:
  ozma-evdev-create <vm-name> [marker-dir]

Creates:
  /dev/input/by-id/ozma-kbd-<vm-name>   → uinput keyboard
  /dev/input/by-id/ozma-mouse-<vm-name> → uinput mouse
  <marker-dir>/<vm-name>.kbd            → contains evdev path
  <marker-dir>/<vm-name>.mouse          → contains evdev path
"""

from __future__ import annotations

import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))

from evdev_input import VirtualKeyboard, VirtualMouse


def create_devices(vm_name: str, marker_dir: str = "/run/ozma/evdev") -> tuple[str, str]:
    """Create evdev devices and write marker files. Returns (kbd_path, mouse_path)."""
    marker_path = Path(marker_dir)
    marker_path.mkdir(parents=True, exist_ok=True)

    kbd = VirtualKeyboard(name=f"ozma-kbd-{vm_name}")
    mouse = VirtualMouse(name=f"ozma-mouse-{vm_name}")

    kbd_path = kbd.start()
    mouse_path = mouse.start()

    if not kbd_path or not mouse_path:
        kbd.stop()
        mouse.stop()
        raise RuntimeError(f"Failed to create evdev devices for {vm_name}")

    # Write marker files so the hook and virtual-node-manager know
    # the devices exist and what paths they're at
    (marker_path / f"{vm_name}.kbd").write_text(kbd_path)
    (marker_path / f"{vm_name}.mouse").write_text(mouse_path)

    # Don't close — the devices must stay alive for QEMU.
    # This process will be kept alive by the hook or adopted by
    # ozma-virtual-node.
    return kbd_path, mouse_path


def main() -> None:
    if len(sys.argv) < 2:
        print(f"Usage: {sys.argv[0]} <vm-name> [marker-dir]", file=sys.stderr)
        sys.exit(1)

    vm_name = sys.argv[1]
    marker_dir = sys.argv[2] if len(sys.argv) > 2 else "/run/ozma/evdev"

    kbd_path, mouse_path = create_devices(vm_name, marker_dir)
    print(f"kbd={kbd_path} mouse={mouse_path}")

    # Stay alive to keep the uinput devices open
    try:
        while True:
            time.sleep(3600)
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
