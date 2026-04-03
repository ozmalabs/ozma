# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
#!/usr/bin/env python3
"""
Ozma Virtual Node — manages all VMs on a QEMU/KVM/Proxmox hypervisor.

One process per hypervisor host. Auto-discovers all running VMs via
libvirt or QMP socket scanning, creates a soft node per VM, and manages
the lifecycle (VM starts → node appears, VM stops → node disappears).

Every new VM automatically gets:
  1. A SoftNode (evdev HID, VNC/D-Bus capture, power control)
  2. Controller registration (mDNS + direct HTTP)
  3. Agent provisioning attempt (qemu-ga → install ozma-agent inside the VM)

Zero config required. Just run `ozma-virtual-node` and every VM on the
host becomes an ozma node.

HID injection via evdev (uinput → QEMU input-linux). Power control via
libvirt API. QMP is not required.

Deployment:
  Proxmox:  apt install ozma-virtual-node && systemctl enable ozma-virtual-node
  libvirt:  uv pip install ozma-virtual-node && ozma-virtual-node
  Manual:   ozma-virtual-node --qmp-dir /tmp/ --vnc-base 5900

Usage:
  ozma-virtual-node                          # auto-detect + auto-manage everything
  ozma-virtual-node --controller http://10.0.0.1:7380
  ozma-virtual-node --exclude 'template-*,infra-*'
  ozma-virtual-node --no-auto-agent          # softnodes only, no agent provisioning
  ozma-virtual-node --no-auto-manage         # discover only, don't create nodes
"""

from __future__ import annotations

import asyncio
import fnmatch
import glob
import json
import logging
import os
import signal
import socket
import sys
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.virtual_node")

# Add parent to path for imports
sys.path.insert(0, str(Path(__file__).parent))

from soft_node import SoftNode
from libvirt_power import LibvirtPower


# ── VM display output ──────────────────────────────────────────────────────

class VMDisplayOutput:
    """A single display output on a VM."""
    def __init__(self, index: int = 0, source_type: str = "dbus",
                 vnc_port: int = 0, dbus_console: int = 0,
                 ivshmem_path: str = "",
                 resolution: tuple[int, int] = (1920, 1080),
                 capture_source_id: str = "") -> None:
        self.index = index
        self.source_type = source_type    # "dbus", "vnc", "ivshmem", "agent"
        self.vnc_port = vnc_port
        self.dbus_console = dbus_console  # D-Bus Console_N index
        self.ivshmem_path = ivshmem_path
        self.resolution = resolution
        self.capture_source_id = capture_source_id

    def to_dict(self) -> dict:
        return {
            "index": self.index,
            "source_type": self.source_type,
            "capture_source_id": self.capture_source_id,
            "width": self.resolution[0],
            "height": self.resolution[1],
        }


# ── VM discovery backends ───────────────────────────────────────────────────

class VMInfo:
    """Discovered VM metadata."""
    def __init__(self, name: str, vm_id: str = "", qmp_path: str = "",
                 vnc_port: int = 0, vnc_host: str = "127.0.0.1",
                 state: str = "running", pid: int = 0,
                 has_guest_agent: bool = False,
                 guest_os: str = "") -> None:
        self.name = name
        self.vm_id = vm_id or name
        self.qmp_path = qmp_path
        self.vnc_port = vnc_port
        self.vnc_host = vnc_host
        self.state = state
        self.pid = pid
        self.has_guest_agent = has_guest_agent
        self.guest_os = guest_os  # "windows", "linux", "" (unknown)
        self.has_gpu_passthrough = False  # True if a GPU is bound to vfio-pci in this VM
        self.displays: list[VMDisplayOutput] = []

    def __repr__(self) -> str:
        return (f"VM({self.name}, vnc=:{self.vnc_port}, "
                f"guest_agent={'yes' if self.has_guest_agent else 'no'}, "
                f"os={self.guest_os or '?'})")


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
        guest_os = ""
        has_gpu_passthrough = False
        if conf_path.exists():
            for line in conf_path.read_text().splitlines():
                if line.startswith("name:"):
                    name = line.split(":", 1)[1].strip()
                if line.startswith("ostype:"):
                    ostype = line.split(":", 1)[1].strip()
                    if ostype.startswith("win"):
                        guest_os = "windows"
                    elif ostype.startswith("l"):
                        guest_os = "linux"
                if line.startswith("args:") and "-vnc" in line:
                    pass  # complex parsing, fall back to default
                # hostpciN: 0000:29:00,x-vga=1 → GPU passthrough
                if line.startswith("hostpci") and "x-vga=1" in line:
                    has_gpu_passthrough = True
            vnc_port = 5900 + int(vmid) if vmid.isdigit() else 0

        vm = VMInfo(
            name=name, vm_id=vmid,
            qmp_path=str(qmp_sock),
            vnc_port=vnc_port,
            state="running",
            has_guest_agent=True,  # Proxmox always installs qemu-ga
            guest_os=guest_os,
        )
        vm.has_gpu_passthrough = has_gpu_passthrough
        vms.append(vm)

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

            xml = domain.XMLDesc()
            qmp_path = ""
            vnc_port = 0
            has_guest_agent = False
            guest_os = ""

            import xml.etree.ElementTree as ET
            root = ET.fromstring(xml)

            # QMP socket from qemu:commandline
            for qmp in root.findall(".//qemu:commandline/qemu:arg",
                                    {"qemu": "http://libvirt.org/schemas/domain/qemu/1.0"}):
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

            # Guest agent channel
            for channel in root.findall(".//channel"):
                target = channel.find("target")
                if target is not None and target.get("name") == "org.qemu.guest_agent.0":
                    has_guest_agent = True

            # GPU passthrough: <hostdev type='pci'> with vfio driver
            has_gpu_passthrough = False
            for hostdev in root.findall(".//hostdev[@type='pci']"):
                driver = hostdev.find("driver")
                if driver is not None and driver.get("name") == "vfio":
                    has_gpu_passthrough = True
                    break
                # Also catch passthrough without explicit driver element —
                # check if the PCI address is bound to vfio-pci on the host
                addr = hostdev.find("source/address")
                if addr is not None:
                    domain = addr.get("domain", "0x0000").replace("0x", "").zfill(4)
                    bus    = addr.get("bus",    "0x00").replace("0x", "").zfill(2)
                    slot   = addr.get("slot",   "0x00").replace("0x", "").zfill(2)
                    func   = addr.get("function", "0x0").replace("0x", "")
                    pci    = f"{domain}:{bus}:{slot}.{func}"
                    driver_link = Path(f"/sys/bus/pci/devices/{pci}/driver")
                    if driver_link.is_symlink() and "vfio" in str(driver_link.resolve()):
                        has_gpu_passthrough = True
                        break

            # Guest OS detection from osinfo
            os_elem = root.find(".//os/type")
            for meta in root.findall(".//{http://libosinfo.org/xmlns/libvirt/domain/1.0}os"):
                os_id = meta.get("id", "")
                if "win" in os_id.lower():
                    guest_os = "windows"
                elif any(x in os_id.lower() for x in ("linux", "ubuntu", "fedora", "debian", "rhel")):
                    guest_os = "linux"

            vm = VMInfo(
                name=name, vm_id=vm_id,
                qmp_path=qmp_path,
                vnc_port=vnc_port,
                state="running",
                has_guest_agent=has_guest_agent,
                guest_os=guest_os,
            )
            vm.has_gpu_passthrough = has_gpu_passthrough
            vms.append(vm)

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
        # Only actual Unix sockets, not regular files
        import stat
        try:
            if not stat.S_ISSOCK(os.stat(sock_path).st_mode):
                continue
        except OSError:
            continue
        # Skip ozma's own sockets and stream directories
        if "/run/ozma/" in sock_path or "ozma-mon" in sock_path or "ozma-stream" in sock_path:
            continue
        name = Path(sock_path).stem.replace("ozma-", "").replace(".qmp", "").replace(".monitor", "")
        vms.append(VMInfo(
            name=name, vm_id=name,
            qmp_path=sock_path,
        ))
    if vms:
        log.info("QMP scan: found %d sockets in %s", len(vms), qmp_dir)
    return vms


# ── Agent provisioning ─────────────────────────────────────────────────────

class AgentProvisioner:
    """
    Attempts to install and start the ozma agent inside a VM.

    Strategy (tried in order):
      1. qemu-ga: run commands via QEMU guest agent channel
      2. SSH: if we can reach the VM's IP (libvirt network)
      3. Virtual USB: present agent installer as a USB drive

    The agent is a single-binary install:
      - Windows: ozma-agent.exe (--install registers as a service)
      - Linux:   uv pip install ozma-agent && systemctl enable ozma-agent
    """

    def __init__(self, controller_url: str = "") -> None:
        self._controller_url = controller_url

    async def provision(self, vm: VMInfo) -> bool:
        """Try to provision the agent inside a VM. Returns True if successful."""
        # Check if agent is already responding
        if await self._agent_alive(vm):
            log.info("Agent already running in %s", vm.name)
            return True

        if vm.has_guest_agent:
            ok = await self._provision_via_guest_agent(vm)
            if ok:
                return True

        log.debug("Agent provisioning not available for %s (no guest agent channel)", vm.name)
        return False

    async def _agent_alive(self, vm: VMInfo) -> bool:
        """Check if the ozma agent is already responding inside the VM."""
        # The agent listens on port 7382 inside the VM. If we can reach
        # the VM's IP (via libvirt bridge or direct network), check it.
        # For now, check via guest-agent ping.
        if not vm.has_guest_agent:
            return False
        try:
            proc = await asyncio.create_subprocess_exec(
                "virsh", "qemu-agent-command", vm.name,
                '{"execute":"guest-ping"}',
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=5)
            if proc.returncode == 0:
                # Guest agent is alive — check if ozma agent process exists
                return await self._check_agent_process(vm)
        except (asyncio.TimeoutError, Exception):
            pass
        return False

    async def _check_agent_process(self, vm: VMInfo) -> bool:
        """Check if ozma-agent is running inside the VM via guest-exec."""
        try:
            if vm.guest_os == "windows":
                cmd = '{"execute":"guest-exec","arguments":{"path":"tasklist","arg":["/FI","IMAGENAME eq ozma-agent.exe"],"capture-output":true}}'
            else:
                cmd = '{"execute":"guest-exec","arguments":{"path":"pgrep","arg":["-f","ozma"],"capture-output":true}}'

            proc = await asyncio.create_subprocess_exec(
                "virsh", "qemu-agent-command", vm.name, cmd,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=5)
            if proc.returncode == 0:
                result = json.loads(stdout.decode())
                pid = result.get("return", {}).get("pid", 0)
                if pid:
                    # Wait for the command to complete and check output
                    await asyncio.sleep(1)
                    status_cmd = f'{{"execute":"guest-exec-status","arguments":{{"pid":{pid}}}}}'
                    proc2 = await asyncio.create_subprocess_exec(
                        "virsh", "qemu-agent-command", vm.name, status_cmd,
                        stdout=asyncio.subprocess.PIPE,
                        stderr=asyncio.subprocess.PIPE,
                    )
                    stdout2, _ = await asyncio.wait_for(proc2.communicate(), timeout=5)
                    if proc2.returncode == 0:
                        status = json.loads(stdout2.decode())
                        exitcode = status.get("return", {}).get("exitcode", -1)
                        return exitcode == 0
        except (asyncio.TimeoutError, Exception) as e:
            log.debug("Agent process check failed for %s: %s", vm.name, e)
        return False

    async def _provision_via_guest_agent(self, vm: VMInfo) -> bool:
        """Install and start the ozma agent via QEMU guest agent."""
        log.info("Provisioning agent in %s via guest agent...", vm.name)

        controller = self._controller_url or "http://10.200.0.1:7380"

        if vm.guest_os == "windows":
            return await self._provision_windows(vm, controller)
        elif vm.guest_os == "linux":
            return await self._provision_linux(vm, controller)
        else:
            # Try Linux first (more common for VMs), fall back to Windows
            if await self._provision_linux(vm, controller):
                return True
            return await self._provision_windows(vm, controller)

    async def _provision_linux(self, vm: VMInfo, controller: str) -> bool:
        """Install ozma-agent on a Linux VM."""
        install_cmd = (
            "uv pip install ozma-agent 2>/dev/null || pip3 install ozma-agent 2>/dev/null; "
            f"ozma-agent --controller {controller} --daemon"
        )
        return await self._guest_exec(vm, "/bin/sh", ["-c", install_cmd])

    async def _provision_windows(self, vm: VMInfo, controller: str) -> bool:
        """Start ozma-agent on a Windows VM (assumes pre-installed)."""
        # Try to start the service if it exists, or run the exe directly
        start_cmd = (
            f'net start ozma-agent 2>nul || '
            f'start /B C:\\ozma-agent\\ozma-agent.exe --controller {controller}'
        )
        return await self._guest_exec(vm, "cmd.exe", ["/C", start_cmd])

    async def _guest_exec(self, vm: VMInfo, path: str, args: list[str]) -> bool:
        """Run a command inside the VM via qemu-ga."""
        try:
            cmd_json = json.dumps({
                "execute": "guest-exec",
                "arguments": {
                    "path": path,
                    "arg": args,
                    "capture-output": True,
                },
            })
            proc = await asyncio.create_subprocess_exec(
                "virsh", "qemu-agent-command", vm.name, cmd_json,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            stdout, stderr = await asyncio.wait_for(proc.communicate(), timeout=30)
            if proc.returncode == 0:
                log.info("Agent provisioning command sent to %s", vm.name)
                return True
            log.debug("guest-exec failed for %s: %s", vm.name, stderr.decode().strip())
        except asyncio.TimeoutError:
            log.debug("guest-exec timed out for %s", vm.name)
        except Exception as e:
            log.debug("guest-exec error for %s: %s", vm.name, e)
        return False


# ── Managed VM state ───────────────────────────────────────────────────────

class ManagedVM:
    """Tracks state for a VM being managed by the virtual node manager."""

    def __init__(self, vm: VMInfo, node: SoftNode, power: LibvirtPower | None = None) -> None:
        self.vm = vm
        self.node = node
        self.power = power
        self.task: asyncio.Task | None = None
        self.agent_provisioned = False
        self.agent_check_failures = 0
        self.evdev_attached = False

    def to_dict(self) -> dict:
        return {
            "name": self.vm.name,
            "port": self.node._port,
            "guest_os": self.vm.guest_os,
            "has_guest_agent": self.vm.has_guest_agent,
            "agent_provisioned": self.agent_provisioned,
            "evdev_attached": self.evdev_attached,
        }


# ── Virtual Node Manager ────────────────────────────────────────────────────

class VirtualNodeManager:
    """
    Manages one soft node + agent per VM on the hypervisor.

    Auto-discovers VMs, creates SoftNode instances, registers with the
    controller, and provisions agents inside the VMs. All on by default.
    """

    def __init__(self, controller_url: str = "", qmp_dir: str = "",
                 base_port: int = 7332, audio_sink_prefix: str = "ozma-",
                 auto_manage: bool = True, auto_agent: bool = True,
                 exclude_patterns: list[str] | None = None) -> None:
        self._controller_url = controller_url
        self._qmp_dir = qmp_dir
        self._base_port = base_port
        self._audio_prefix = audio_sink_prefix
        self._auto_manage = auto_manage
        self._auto_agent = auto_agent
        self._exclude = exclude_patterns or []
        self._managed: dict[str, ManagedVM] = {}
        self._provisioner = AgentProvisioner(controller_url)
        self._stop_event = asyncio.Event()
        self._next_port = base_port

    async def run(self) -> None:
        """Discover VMs and start managing them."""
        log.info("Virtual Node Manager starting...")
        log.info("  Controller: %s", self._controller_url or "(auto-discover)")
        log.info("  Auto-manage: %s", "on" if self._auto_manage else "off")
        log.info("  Auto-agent:  %s", "on" if self._auto_agent else "off")
        if self._exclude:
            log.info("  Exclude:     %s", ", ".join(self._exclude))

        # Initial discovery
        await self._discover_and_sync()

        # Watch for changes
        asyncio.create_task(self._watch_loop(), name="vm-watcher")

        # Agent provisioning loop (separate cadence)
        if self._auto_agent:
            asyncio.create_task(self._agent_loop(), name="agent-provisioner")

        # Wait for stop
        await self._stop_event.wait()

    async def stop(self) -> None:
        self._stop_event.set()
        for managed in self._managed.values():
            if managed.task:
                managed.task.cancel()

    def _is_excluded(self, name: str) -> bool:
        return any(fnmatch.fnmatch(name, pat) for pat in self._exclude)

    async def _discover_and_sync(self) -> None:
        """Discover VMs and start/stop nodes as needed."""
        if not self._auto_manage:
            return

        vms = self._discover_vms()
        current_names = {vm.name for vm in vms}
        managed_names = set(self._managed.keys())

        # Start nodes for new VMs
        for vm in vms:
            if vm.name not in managed_names and not self._is_excluded(vm.name):
                await self._start_node(vm)

        # Stop nodes for VMs that no longer exist
        for name in managed_names - current_names:
            await self._stop_node(name)

    def _discover_vms(self) -> list[VMInfo]:
        """Run all discovery backends and merge results."""
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

    def _is_proxmox(self) -> bool:
        """Detect if running on a Proxmox VE host."""
        return Path("/var/run/qemu-server").exists()

    async def _provision_vm(self, vm: VMInfo) -> str:
        """Ensure the VM has an ozma QMP socket. Returns QMP socket path or ''."""
        # On Proxmox, the Perl hook handles QEMU args (D-Bus display, KVMFR, audio).
        # The QMP socket is the Proxmox-native one at /var/run/qemu-server/VMID.qmp.
        # We don't modify the VM config — just use what Proxmox provides.
        if self._is_proxmox():
            qmp = vm.qmp_path
            if qmp and os.path.exists(qmp):
                log.info("Proxmox VM %s: using native QMP %s", vm.name, qmp)
                return qmp
            return ""

        # Libvirt VMs: provision our own QMP socket + D-Bus display
        qmp_dir = "/run/ozma/qmp"
        os.makedirs(qmp_dir, mode=0o775, exist_ok=True)
        qmp_sock = f"{qmp_dir}/{vm.name}.sock"

        # Check if already provisioned and running
        if os.path.exists(qmp_sock):
            return qmp_sock

        # Check if the VM XML already has ozma config
        try:
            proc = await asyncio.create_subprocess_exec(
                "virsh", "dumpxml", "--inactive", vm.name,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL,
            )
            stdout, _ = await proc.communicate()
            if b"ozma-mon" in stdout:
                return qmp_sock  # already provisioned
        except Exception:
            return ""

        # Add ozma QMP socket + D-Bus p2p display to the VM's config.
        # - QMP socket: direct control channel (input, screendump)
        # - D-Bus p2p: QEMU serves display, we connect via QMP add_client
        #   No bus daemon needed — everything goes through the QMP socket.
        log.info("Provisioning ozma integration for %s", vm.name)
        try:
            xml = stdout.decode()

            # PipeWire audio sink name for this VM
            pw_sink = f"ozma-{vm.name}"

            # Request evdev devices from the evdev service
            kbd_path = ""
            mouse_path = ""
            try:
                from evdev_service import create_devices
                result = await create_devices(vm.name)
                kbd_path = result.get("kbd", "")
                mouse_path = result.get("mouse", "")
                if kbd_path:
                    log.info("evdev devices for %s: kbd=%s mouse=%s", vm.name, kbd_path, mouse_path)
            except Exception as e:
                log.debug("evdev service not available: %s", e)

            # Second QMP socket for D-Bus display setup (add_client + getfd)
            qmp_display_sock = f"{qmp_dir}/{vm.name}-display.sock"

            qemu_block = (
                "  <qemu:commandline>\n"
                # QMP control socket (input, screendump, power)
                "    <qemu:arg value='-chardev'/>\n"
                f"    <qemu:arg value='socket,id=ozma-mon,path={qmp_sock},server=on,wait=off'/>\n"
                "    <qemu:arg value='-mon'/>\n"
                "    <qemu:arg value='chardev=ozma-mon,mode=control'/>\n"
                # QMP display socket (D-Bus add_client)
                "    <qemu:arg value='-chardev'/>\n"
                f"    <qemu:arg value='socket,id=ozma-display,path={qmp_display_sock},server=on,wait=off'/>\n"
                "    <qemu:arg value='-mon'/>\n"
                "    <qemu:arg value='chardev=ozma-display,mode=control'/>\n"
                # D-Bus p2p display
                "    <qemu:arg value='-display'/>\n"
                "    <qemu:arg value='dbus,p2p=yes'/>\n"
            )
            # evdev input-linux (if evdev service provided devices)
            if kbd_path and mouse_path:
                qemu_block += (
                    f"    <qemu:arg value='-object'/>\n"
                    f"    <qemu:arg value='input-linux,id=ozma-kbd,evdev={kbd_path}'/>\n"
                    f"    <qemu:arg value='-object'/>\n"
                    f"    <qemu:arg value='input-linux,id=ozma-mouse,evdev={mouse_path},grab_all=on'/>\n"
                )
            # Environment for audio + display
            qemu_block += (
                "    <qemu:env name='PULSE_SERVER' value='unix:/run/user/1000/pulse/native'/>\n"
                "    <qemu:env name='XDG_RUNTIME_DIR' value='/run/user/1000'/>\n"
            )
            qemu_block += "  </qemu:commandline>"

            ns_decl = "xmlns:qemu='http://libvirt.org/schemas/domain/qemu/1.0'"
            if ns_decl not in xml:
                xml = xml.replace("type='kvm'", f"type='kvm' {ns_decl}", 1)
            # Replace audio none with PulseAudio (PipeWire serves this)
            # PulseAudio works better than native PipeWire for cross-user access
            xml = xml.replace(
                "<audio id='1' type='none'/>",
                "<audio id='1' type='pulseaudio'/>"
            )
            xml = xml.replace("</domain>", f"{qemu_block}\n</domain>")

            proc = await asyncio.create_subprocess_exec(
                "virsh", "define", "/dev/stdin",
                stdin=asyncio.subprocess.PIPE,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE,
            )
            _, stderr = await proc.communicate(input=xml.encode())
            if proc.returncode != 0:
                log.warning("Failed to provision QMP for %s: %s", vm.name, stderr.decode().strip())
                return ""

            log.info("QMP socket provisioned for %s — restart VM to activate", vm.name)
            # Restart the VM to pick up the new config
            proc = await asyncio.create_subprocess_exec(
                "virsh", "destroy", vm.name,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.DEVNULL,
            )
            await proc.communicate()
            await asyncio.sleep(1)
            proc = await asyncio.create_subprocess_exec(
                "virsh", "start", vm.name,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.DEVNULL,
            )
            await proc.communicate()
            if proc.returncode == 0:
                log.info("VM %s restarted with QMP socket", vm.name)
                await asyncio.sleep(3)  # let it boot
                return sock_path
            return ""
        except Exception as e:
            log.warning("QMP provisioning failed for %s: %s", vm.name, e)
            return ""

    async def _start_node(self, vm: VMInfo) -> None:
        """Create and start a SoftNode for a VM."""
        port = self._next_port
        self._next_port += 1

        # Ensure VM has ozma QMP socket + D-Bus display
        qmp_sock = await self._provision_vm(vm)


        # Libvirt power backend (preferred over QMP)
        power = LibvirtPower(vm.name)
        await power.start()

        # GPU passthrough VMs have no virtual display (no VNC, no D-Bus).
        # The ozma agent inside the VM captures the real GPU framebuffer and
        # pushes frames directly — VNC port is meaningless, don't pass it.
        if vm.has_gpu_passthrough:
            vnc_port = None
            log.info("VM %s has GPU passthrough — skipping VNC, using agent display", vm.name)
        else:
            vnc_port = vm.vnc_port if vm.vnc_port else None

        # Use the ozma QMP socket for input/display, libvirt for power
        qmp = qmp_sock if qmp_sock else (vm.qmp_path if not power.connected else "")

        node = SoftNode(
            name=vm.name,
            host="0.0.0.0",
            port=port,
            qmp_path=qmp,
            vnc_host=vm.vnc_host,
            vnc_port=vnc_port,
            audio_sink=f"{self._audio_prefix}{vm.name}",
            api_port=7380 + port - self._base_port + 2,
            power_backend=power,
        )

        managed = ManagedVM(vm, node, power)
        managed.task = asyncio.create_task(
            self._run_node(vm.name, node),
            name=f"vnode-{vm.name}",
        )
        self._managed[vm.name] = managed
        log.info("Auto-managed VM: %s (qmp=%s, port=%d, display=%s, os=%s)",
                 vm.name, "direct" if qmp_sock else "libvirt", port,
                 "agent-capture" if vm.has_gpu_passthrough else f"vnc:{vnc_port or 'none'}",
                 vm.guest_os or "unknown")

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
        managed = self._managed.pop(name, None)
        if managed:
            if managed.task:
                managed.task.cancel()
            await managed.node.stop()
            if managed.power:
                await managed.power.close()
        log.info("Stopped virtual node: %s", name)

    async def _watch_loop(self) -> None:
        """Periodically re-scan for VM changes."""
        while not self._stop_event.is_set():
            await asyncio.sleep(10)
            try:
                await self._discover_and_sync()
            except Exception as e:
                log.debug("VM watch error: %s", e)

    async def _agent_loop(self) -> None:
        """Periodically attempt agent provisioning for VMs that don't have it."""
        # Wait for nodes to stabilise before first attempt
        await asyncio.sleep(15)

        while not self._stop_event.is_set():
            for managed in list(self._managed.values()):
                if managed.agent_provisioned:
                    continue
                # GPU passthrough VMs: agent is the ONLY display path, never give up.
                # Other VMs: stop retrying after 5 failures (VNC/D-Bus will cover them).
                max_failures = None if managed.vm.has_gpu_passthrough else 5
                if max_failures is not None and managed.agent_check_failures > max_failures:
                    continue
                try:
                    ok = await self._provisioner.provision(managed.vm)
                    if ok:
                        managed.agent_provisioned = True
                        log.info("Agent provisioned in %s", managed.vm.name)
                    else:
                        managed.agent_check_failures += 1
                        if managed.vm.has_gpu_passthrough:
                            log.debug(
                                "GPU passthrough VM %s: agent not yet installed "
                                "(attempt %d) — will keep retrying",
                                managed.vm.name, managed.agent_check_failures,
                            )
                except Exception as e:
                    log.debug("Agent provisioning error for %s: %s", managed.vm.name, e)
                    managed.agent_check_failures += 1

            await asyncio.sleep(30)

    def status(self) -> dict:
        return {
            "auto_manage": self._auto_manage,
            "auto_agent": self._auto_agent,
            "managed_vms": len(self._managed),
            "vms": [m.to_dict() for m in self._managed.values()],
        }


# ── CLI ─────────────────────────────────────────────────────────────────────

def main() -> None:
    import argparse

    p = argparse.ArgumentParser(
        description="Ozma Virtual Node — auto-manage all VMs on a hypervisor",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Every running VM automatically gets a soft node (HID + capture + power)
and an agent provisioning attempt. This is on by default.

Examples:
  ozma-virtual-node                                    # auto-detect + manage everything
  ozma-virtual-node --controller http://10.0.0.1:7380  # explicit controller
  ozma-virtual-node --exclude 'template-*,infra-*'     # skip matching VMs
  ozma-virtual-node --no-auto-agent                    # softnodes only, skip agent
  ozma-virtual-node --no-auto-manage                   # discover only, don't manage
""")
    p.add_argument("--controller", default="", help="Controller URL")
    p.add_argument("--qmp-dir", default="", help="Directory to scan for QMP sockets")
    p.add_argument("--base-port", type=int, default=7332, help="Starting UDP port")
    p.add_argument("--exclude", default="",
                   help="Comma-separated VM name patterns to exclude (e.g. 'template-*,infra-*')")
    p.add_argument("--no-auto-manage", action="store_true",
                   help="Don't auto-create nodes for discovered VMs")
    p.add_argument("--no-auto-agent", action="store_true",
                   help="Don't attempt agent provisioning inside VMs")
    p.add_argument("--debug", action="store_true")
    args = p.parse_args()

    logging.basicConfig(
        level=logging.DEBUG if args.debug else logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
        datefmt="%H:%M:%S",
    )

    exclude = [p.strip() for p in args.exclude.split(",") if p.strip()] if args.exclude else []

    mgr = VirtualNodeManager(
        controller_url=args.controller,
        qmp_dir=args.qmp_dir,
        base_port=args.base_port,
        auto_manage=not args.no_auto_manage,
        auto_agent=not args.no_auto_agent,
        exclude_patterns=exclude,
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
