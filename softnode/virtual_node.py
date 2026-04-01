# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
#!/usr/bin/env python3
"""
Ozma Virtual Node — manages all VMs on a QEMU/KVM/Proxmox hypervisor.

One process per hypervisor host. Auto-discovers all running VMs via
libvirt or QMP socket scanning, creates a soft node per VM, and manages
the lifecycle (VM starts → node appears, VM stops → node disappears).

Zero software inside the VMs. HID injection via QMP. Display capture
via VNC or SPICE. Audio via QEMU audio device.

Deployment:
  Proxmox:  apt install ozma-virtual-node && systemctl enable ozma-virtual-node
  libvirt:  pip install ozma-virtual-node && ozma-virtual-node
  Manual:   ozma-virtual-node --qmp-dir /tmp/ --vnc-base 5900

Supported hypervisors:
  - Proxmox VE (libvirt + QMP, auto-detected)
  - QEMU/KVM via libvirt
  - QEMU/KVM via QMP sockets (no libvirt needed)
  - Potentially VMware/Hyper-V in the future (different HID path)

Usage:
  ozma-virtual-node                          # auto-detect everything
  ozma-virtual-node --controller http://10.0.0.1:7380
  ozma-virtual-node --qmp-dir /var/run/qemu-server/  # Proxmox QMP sockets
  ozma-virtual-node --libvirt qemu:///system
"""

from __future__ import annotations

import asyncio
import glob
import json
import logging
import os
import platform
import signal
import socket
import sys
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.virtual_node")

# Add parent to path for imports
sys.path.insert(0, str(Path(__file__).parent))

from soft_node import SoftNode


# ── VM discovery backends ───────────────────────────────────────────────────

class VMInfo:
    """Discovered VM metadata."""
    def __init__(self, name: str, vm_id: str = "", qmp_path: str = "",
                 vnc_port: int = 0, vnc_host: str = "127.0.0.1",
                 state: str = "running", pid: int = 0) -> None:
        self.name = name
        self.vm_id = vm_id or name
        self.qmp_path = qmp_path
        self.vnc_port = vnc_port
        self.vnc_host = vnc_host
        self.state = state
        self.pid = pid

    def __repr__(self) -> str:
        return f"VM({self.name}, qmp={self.qmp_path}, vnc=:{self.vnc_port})"


def discover_proxmox_vms() -> list[VMInfo]:
    """Discover VMs on a Proxmox VE host via QMP sockets."""
    vms = []
    # Proxmox stores QMP sockets in /var/run/qemu-server/
    qmp_dir = Path("/var/run/qemu-server")
    if not qmp_dir.exists():
        return vms

    for qmp_sock in sorted(qmp_dir.glob("*.qmp")):
        vmid = qmp_sock.stem  # e.g., "100" from "100.qmp"
        # Get VM name from Proxmox config
        conf_path = Path(f"/etc/pve/qemu-server/{vmid}.conf")
        name = vmid
        vnc_port = 0
        if conf_path.exists():
            for line in conf_path.read_text().splitlines():
                if line.startswith("name:"):
                    name = line.split(":", 1)[1].strip()
                # Proxmox VNC port = 5900 + display number
                # Display number is typically VMID for Proxmox
                if line.startswith("args:") and "-vnc" in line:
                    pass  # complex parsing, fall back to default
            vnc_port = 5900 + int(vmid) if vmid.isdigit() else 0

        vms.append(VMInfo(
            name=name, vm_id=vmid,
            qmp_path=str(qmp_sock),
            vnc_port=vnc_port,
            state="running",
        ))

    log.info("Proxmox: discovered %d VMs", len(vms))
    return vms


def discover_libvirt_vms() -> list[VMInfo]:
    """Discover VMs via libvirt."""
    vms = []
    try:
        import libvirt
        conn = libvirt.openReadOnly("qemu:///system")
        if not conn:
            return vms

        for domain in conn.listAllDomains():
            if not domain.isActive():
                continue
            name = domain.name()
            vm_id = str(domain.ID())

            # Find QMP socket
            xml = domain.XMLDesc()
            qmp_path = ""
            vnc_port = 0

            # Parse QMP path from domain XML
            import xml.etree.ElementTree as ET
            root = ET.fromstring(xml)

            # QMP socket
            for qmp in root.findall(".//qemu:commandline/qemu:arg", {"qemu": "http://libvirt.org/schemas/domain/qemu/1.0"}):
                val = qmp.get("value", "")
                if val.startswith("unix:") and "qmp" in val.lower():
                    qmp_path = val.split("unix:")[1].split(",")[0]

            # Also check common paths
            if not qmp_path:
                for candidate in [
                    f"/var/lib/libvirt/qemu/domain-{vm_id}-{name}/monitor.sock",
                    f"/tmp/qemu-{name}.qmp",
                    f"/var/run/libvirt/qemu/{name}.monitor",
                ]:
                    if Path(candidate).exists():
                        qmp_path = candidate
                        break

            # VNC port from XML
            for graphics in root.findall(".//graphics[@type='vnc']"):
                port = graphics.get("port", "-1")
                if port and port != "-1":
                    vnc_port = int(port)

            vms.append(VMInfo(
                name=name, vm_id=vm_id,
                qmp_path=qmp_path,
                vnc_port=vnc_port,
                state="running",
            ))

        conn.close()
        log.info("libvirt: discovered %d VMs", len(vms))
    except ImportError:
        log.debug("libvirt not available")
    except Exception as e:
        log.debug("libvirt discovery failed: %s", e)

    return vms


def discover_qmp_sockets(qmp_dir: str = "/tmp") -> list[VMInfo]:
    """Discover VMs by scanning for QMP sockets in a directory."""
    vms = []
    for sock_path in sorted(glob.glob(f"{qmp_dir}/*qmp*") + glob.glob(f"{qmp_dir}/*.monitor")):
        if not os.path.exists(sock_path):
            continue
        # Try to determine VM name from socket filename
        name = Path(sock_path).stem.replace("ozma-", "").replace(".qmp", "").replace(".monitor", "")
        vms.append(VMInfo(
            name=name, vm_id=name,
            qmp_path=sock_path,
        ))
    if vms:
        log.info("QMP scan: found %d sockets in %s", len(vms), qmp_dir)
    return vms


# ── Virtual Node Manager ────────────────────────────────────────────────────

class VirtualNodeManager:
    """
    Manages one soft node per VM on the hypervisor.

    Auto-discovers VMs, creates SoftNode instances for each,
    and watches for VMs starting/stopping.
    """

    def __init__(self, controller_url: str = "", qmp_dir: str = "",
                 base_port: int = 7332, audio_sink_prefix: str = "ozma-") -> None:
        self._controller_url = controller_url
        self._qmp_dir = qmp_dir
        self._base_port = base_port
        self._audio_prefix = audio_sink_prefix
        self._nodes: dict[str, SoftNode] = {}
        self._node_tasks: dict[str, asyncio.Task] = {}
        self._stop_event = asyncio.Event()
        self._next_port = base_port

    async def run(self) -> None:
        """Discover VMs and start managing them."""
        log.info("Virtual Node Manager starting...")
        log.info("  Controller: %s", self._controller_url or "(auto-discover)")

        # Initial discovery
        await self._discover_and_sync()

        # Watch for changes
        asyncio.create_task(self._watch_loop(), name="vm-watcher")

        # Wait for stop
        await self._stop_event.wait()

    async def stop(self) -> None:
        self._stop_event.set()
        for name, task in self._node_tasks.items():
            task.cancel()

    async def _discover_and_sync(self) -> None:
        """Discover VMs and start/stop nodes as needed."""
        vms = self._discover_vms()
        current_names = {vm.name for vm in vms}
        managed_names = set(self._nodes.keys())

        # Start nodes for new VMs
        for vm in vms:
            if vm.name not in managed_names:
                await self._start_node(vm)

        # Stop nodes for VMs that no longer exist
        for name in managed_names - current_names:
            await self._stop_node(name)

    def _discover_vms(self) -> list[VMInfo]:
        """Run all discovery backends and merge results."""
        vms: list[VMInfo] = []

        # Try Proxmox first (most specific)
        proxmox = discover_proxmox_vms()
        if proxmox:
            return proxmox

        # Try libvirt
        libvirt_vms = discover_libvirt_vms()
        if libvirt_vms:
            return libvirt_vms

        # Fall back to QMP socket scanning
        qmp_dir = self._qmp_dir or "/tmp"
        return discover_qmp_sockets(qmp_dir)

    async def _start_node(self, vm: VMInfo) -> None:
        """Create and start a SoftNode for a VM."""
        if not vm.qmp_path:
            log.warning("VM %s has no QMP socket — skipping", vm.name)
            return

        port = self._next_port
        self._next_port += 1

        node = SoftNode(
            name=vm.name,
            host="0.0.0.0",
            port=port,
            qmp_path=vm.qmp_path,
            vnc_host=vm.vnc_host,
            vnc_port=vm.vnc_port if vm.vnc_port else None,
            audio_sink=f"{self._audio_prefix}{vm.name}",
            api_port=7380 + port - self._base_port + 2,
        )

        self._nodes[vm.name] = node
        task = asyncio.create_task(
            self._run_node(vm.name, node),
            name=f"vnode-{vm.name}",
        )
        self._node_tasks[vm.name] = task
        log.info("Started virtual node: %s (QMP=%s, port=%d, VNC=:%s)",
                 vm.name, vm.qmp_path, port, vm.vnc_port or "none")

    async def _run_node(self, name: str, node: SoftNode) -> None:
        """Run a soft node with restart on failure."""
        while not self._stop_event.is_set():
            try:
                await node.run()
            except asyncio.CancelledError:
                return
            except Exception as e:
                log.warning("Virtual node %s crashed: %s — restarting in 5s", name, e)
                await asyncio.sleep(5)

    async def _stop_node(self, name: str) -> None:
        """Stop a soft node for a VM that went away."""
        task = self._node_tasks.pop(name, None)
        if task:
            task.cancel()
        node = self._nodes.pop(name, None)
        if node:
            await node.stop()
        log.info("Stopped virtual node: %s", name)

    async def _watch_loop(self) -> None:
        """Periodically re-scan for VM changes."""
        while not self._stop_event.is_set():
            await asyncio.sleep(10)
            try:
                await self._discover_and_sync()
            except Exception as e:
                log.debug("VM watch error: %s", e)

    def status(self) -> dict:
        return {
            "managed_vms": len(self._nodes),
            "vms": [
                {"name": name, "port": node._port}
                for name, node in self._nodes.items()
            ],
        }


# ── CLI ─────────────────────────────────────────────────────────────────────

def main() -> None:
    import argparse

    p = argparse.ArgumentParser(
        description="Ozma Virtual Node — manage all VMs on a hypervisor",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Examples:
  ozma-virtual-node                                    # auto-detect everything
  ozma-virtual-node --controller http://10.0.0.1:7380  # explicit controller
  ozma-virtual-node --qmp-dir /var/run/qemu-server/    # Proxmox
  ozma-virtual-node --qmp-dir /tmp/                    # dev/test QEMU VMs
""")
    p.add_argument("--controller", default="", help="Controller URL")
    p.add_argument("--qmp-dir", default="", help="Directory to scan for QMP sockets")
    p.add_argument("--base-port", type=int, default=7332, help="Starting UDP port")
    p.add_argument("--debug", action="store_true")
    args = p.parse_args()

    logging.basicConfig(
        level=logging.DEBUG if args.debug else logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
        datefmt="%H:%M:%S",
    )

    mgr = VirtualNodeManager(
        controller_url=args.controller,
        qmp_dir=args.qmp_dir,
        base_port=args.base_port,
    )

    loop = asyncio.new_event_loop()

    def _on_signal():
        loop.call_soon_threadsafe(mgr._stop_event.set)

    for sig in (signal.SIGINT, signal.SIGTERM):
        loop.add_signal_handler(sig, _on_signal)

    try:
        loop.run_until_complete(mgr.run())
    except KeyboardInterrupt:
        pass
    finally:
        loop.run_until_complete(mgr.stop())
        loop.close()


if __name__ == "__main__":
    main()
