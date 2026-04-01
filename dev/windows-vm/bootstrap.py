#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Windows agent bootstrap orchestrator.

Launches a Windows VM, waits for it to reach the desktop, then uses
ozma's RPA automation engine to install Python, build the agent .exe,
and install it as a service — all via HID input to the VM.

This is the full dogfooding loop:
  ozma controller → soft node → VM → RPA types commands → agent built
  → agent connects back to controller

Usage:
  python3 dev/windows-vm/bootstrap.py --controller https://ozma.hrdwrbob.net

Prerequisites:
  - Windows qcow2 image (dev/windows-vm/provision.sh create first)
  - Controller running
  - dummy_hcd loaded (for full USB path) or QMP available (for emulated path)
"""

from __future__ import annotations

import argparse
import asyncio
import json
import logging
import os
import sys
import time
import urllib.request
from pathlib import Path

REPO_DIR = Path(__file__).parent.parent.parent
sys.path.insert(0, str(REPO_DIR / "softnode"))
sys.path.insert(0, str(REPO_DIR / "controller"))

log = logging.getLogger("ozma.bootstrap")


async def main(controller_url: str, vm_name: str = "win10") -> None:
    images_dir = REPO_DIR / "images"
    disk_image = images_dir / f"{vm_name}.qcow2"
    qmp_socket = f"/tmp/ozma-{vm_name}.qmp"
    vnc_port = 5931
    softnode_port = 7340
    script_path = Path(__file__).parent / "bootstrap-agent.ozma"

    if not disk_image.exists():
        log.error("No Windows disk image. Run: bash dev/windows-vm/provision.sh create")
        return

    # Read and patch the RPA script with the actual controller URL
    script = script_path.read_text()
    script = script.replace("CONTROLLER_URL_PLACEHOLDER", controller_url)

    log.info("=== Windows Agent Bootstrap ===")
    log.info("Controller: %s", controller_url)
    log.info("VM: %s", vm_name)
    log.info("Disk: %s", disk_image)

    # ── Step 1: Launch VM (if not already running) ─────────────────────
    pid_file = Path(f"/tmp/ozma-{vm_name}.pid")
    if pid_file.exists():
        try:
            pid = int(pid_file.read_text().strip())
            os.kill(pid, 0)  # check if running
            log.info("VM already running (PID %d)", pid)
        except (ProcessLookupError, ValueError):
            pid_file.unlink()

    if not pid_file.exists():
        log.info("Starting VM...")
        proc = await asyncio.create_subprocess_exec(
            "bash", str(Path(__file__).parent / "provision.sh"), "start",
            env={**os.environ,
                 "VM_NAME": vm_name,
                 "CONTROLLER_URL": controller_url},
        )
        await proc.wait()
        await asyncio.sleep(5)

    # ── Step 2: Connect soft node to the VM ────────────────────────────
    from qmp_client import QMPClient
    qmp = QMPClient(qmp_socket)
    await qmp.start()

    if not qmp.connected:
        log.error("Cannot connect to QMP at %s", qmp_socket)
        return

    log.info("QMP connected")

    # ── Step 3: Wait for Windows desktop ───────────────────────────────
    log.info("Waiting for Windows to boot to desktop...")
    log.info("(Watch VNC at localhost:%d or the dashboard)", vnc_port)

    # Poll VM status — wait for it to be running
    for i in range(120):  # 10 minutes
        status = await qmp.query_status()
        if status and status.get("status") == "running":
            break
        await asyncio.sleep(5)

    # Give Windows time to reach the desktop after boot
    log.info("VM is running. Waiting 60s for Windows to reach desktop...")
    await asyncio.sleep(60)

    # ── Step 4: Run the RPA bootstrap script ───────────────────────────
    log.info("Running RPA bootstrap script via automation engine...")

    # We need to send HID packets to the VM. The soft node handles this
    # via QMP. Let's use the automation engine directly.
    from automation import AutomationEngine
    from state import AppState, NodeInfo

    # Create a minimal state with our VM as the active node
    state = AppState()
    node = NodeInfo(
        id=f"{vm_name}._ozma._udp.local.",
        host="127.0.0.1",
        port=softnode_port,
        role="compute",
        hw="soft",
        fw_version="1.0.0",
        proto_version=1,
    )
    await state.add_node(node)
    state.active_node_id = node.id

    engine = AutomationEngine(state)

    log.info("Executing bootstrap script (%d lines)...", len(script.splitlines()))
    result = await engine.run_script(script, node_id=node.id)

    if result.get("ok"):
        log.info("RPA script completed")
    else:
        log.warning("RPA script finished with errors: %s", result.get("errors", []))

    # ── Step 5: Wait for agent to register ─────────────────────────────
    log.info("Waiting for agent to register with controller...")

    for i in range(60):  # 5 minutes
        try:
            url = f"{controller_url.rstrip('/')}/api/v1/nodes"
            with urllib.request.urlopen(url, timeout=5) as r:
                data = json.loads(r.read())
                for n in data.get("nodes", []):
                    hw = n.get("hw", "").lower()
                    if "desktop" in hw or "windows" in hw:
                        log.info("=== AGENT CONNECTED ===")
                        log.info("Node: %s (hw: %s)", n["id"], n["hw"])
                        log.info("")
                        log.info("The full loop works:")
                        log.info("  ozma controller")
                        log.info("  → soft node (QMP)")
                        log.info("  → Windows VM")
                        log.info("  → RPA typed commands")
                        log.info("  → Python installed")
                        log.info("  → ozma-agent built + installed")
                        log.info("  → agent registered back with controller")
                        log.info("")
                        log.info("Built .exe should be at C:\\ozma-agent.exe in the VM")
                        return
        except Exception:
            pass
        await asyncio.sleep(5)

    log.warning("Agent didn't register within timeout.")
    log.info("Check the VM via VNC at localhost:%d", vnc_port)
    log.info("Or via dashboard: %s", controller_url)


if __name__ == "__main__":
    p = argparse.ArgumentParser(description="Bootstrap Windows agent via ozma RPA")
    p.add_argument("--controller", default="https://ozma.hrdwrbob.net",
                   help="Controller URL")
    p.add_argument("--vm-name", default="win10", help="VM name")
    p.add_argument("--debug", action="store_true")
    args = p.parse_args()

    logging.basicConfig(
        level=logging.DEBUG if args.debug else logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
        datefmt="%H:%M:%S",
    )

    asyncio.run(main(args.controller, args.vm_name))
