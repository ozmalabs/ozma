#!/usr/bin/python3
# SPDX-License-Identifier: GPL-2.0-or-later
"""
VM Profiles — auto-tuned QEMU configurations for different workloads.

Profiles detect hardware capabilities and generate optimal QEMU arguments.
The hard VFIO tuning (ReBAR, CPU pinning, hugepages, IOMMU, MSI) that
normally takes a week of trial and error becomes a single profile choice.

Profiles:
  gaming    — maximum performance: ReBAR, CPU pinning, hugepages, MSI,
              Looking Glass display, dedicated USB controller, low-latency audio
  workstation — balanced: virtio-gpu, multi-monitor, standard audio
  server    — minimal: headless, serial console, no GPU
  media     — media consumption: virtio-gpu, surround audio, hardware decode

Usage:
  profile = VMProfile.detect("gaming", vmid=100, gpu="0000:01:00.0")
  qemu_args = profile.qemu_args()
  proxmox_conf = profile.proxmox_conf()
"""

from __future__ import annotations

import json
import logging
import os
import re
import subprocess
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.proxmox.vm_profiles")


@dataclass
class CPUTopology:
    """Detected CPU topology for optimal pinning."""
    total_cores: int = 0
    total_threads: int = 0
    sockets: int = 1
    cores_per_socket: int = 0
    threads_per_core: int = 1
    numa_nodes: int = 1
    # core_id → [thread_ids] mapping
    core_threads: dict[int, list[int]] = field(default_factory=dict)
    # NUMA node → [core_ids]
    numa_cores: dict[int, list[int]] = field(default_factory=dict)

    @classmethod
    def detect(cls) -> "CPUTopology":
        topo = cls()
        try:
            out = subprocess.check_output(["lscpu", "-J"], text=True)
            data = json.loads(out)
            for item in data.get("lscpu", []):
                field_name = item.get("field", "").rstrip(":")
                val = item.get("data", "")
                if field_name == "CPU(s)":
                    topo.total_threads = int(val)
                elif field_name == "Core(s) per socket":
                    topo.cores_per_socket = int(val)
                elif field_name == "Socket(s)":
                    topo.sockets = int(val)
                elif field_name == "Thread(s) per core":
                    topo.threads_per_core = int(val)
                elif field_name == "NUMA node(s)":
                    topo.numa_nodes = int(val)
            topo.total_cores = topo.sockets * topo.cores_per_socket

            # Build core→thread mapping from /sys
            for cpu_dir in sorted(Path("/sys/devices/system/cpu").glob("cpu[0-9]*")):
                cpu_id = int(cpu_dir.name[3:])
                try:
                    core_id = int((cpu_dir / "topology/core_id").read_text().strip())
                    topo.core_threads.setdefault(core_id, []).append(cpu_id)
                except (FileNotFoundError, ValueError):
                    pass
                try:
                    node_id = int(next(cpu_dir.glob("node*/")).name[4:])
                    topo.numa_cores.setdefault(node_id, []).append(cpu_id)
                except (StopIteration, ValueError):
                    pass

        except Exception as e:
            log.debug("CPU topology detection failed: %s", e)
        return topo

    def pin_cores(self, num_cores: int, reserve_host: int = 2) -> list[int]:
        """
        Select optimal cores for VM pinning.

        Picks physical cores (with their HT siblings) from the end of the
        core list, leaving the first `reserve_host` cores for the host.
        Returns a list of thread IDs to pin.
        """
        available = sorted(self.core_threads.keys())
        # Reserve first N cores for host
        vm_cores = available[reserve_host:reserve_host + num_cores]
        threads = []
        for core in vm_cores:
            threads.extend(sorted(self.core_threads.get(core, [])))
        return threads


@dataclass
class GPUInfo:
    """Detected GPU capabilities."""
    pci_address: str = ""   # e.g. "0000:01:00.0"
    vendor: str = ""        # nvidia, amd, intel
    model: str = ""
    vram_mb: int = 0
    rebar_supported: bool = False
    rebar_enabled: bool = False
    sriov_supported: bool = False
    iommu_group: int = -1
    # All devices in the same IOMMU group (must all be passed through)
    iommu_group_devices: list[str] = field(default_factory=list)
    # Audio device on the same GPU (e.g. HDMI audio)
    audio_pci: str = ""

    @classmethod
    def detect(cls, pci_address: str) -> "GPUInfo":
        gpu = cls(pci_address=pci_address)
        try:
            # lspci for model
            out = subprocess.check_output(["lspci", "-s", pci_address, "-v"], text=True)
            for line in out.splitlines():
                if "VGA" in line or "3D" in line:
                    gpu.model = line.split(":", 2)[-1].strip()
                if "NVIDIA" in line.upper():
                    gpu.vendor = "nvidia"
                elif "AMD" in line.upper() or "ATI" in line.upper():
                    gpu.vendor = "amd"
                elif "INTEL" in line.upper():
                    gpu.vendor = "intel"

            # IOMMU group
            iommu_path = Path(f"/sys/bus/pci/devices/{pci_address}/iommu_group")
            if iommu_path.is_symlink():
                gpu.iommu_group = int(os.path.basename(os.readlink(str(iommu_path))))
                # Find all devices in the same group
                group_path = Path(f"/sys/kernel/iommu_groups/{gpu.iommu_group}/devices")
                if group_path.exists():
                    gpu.iommu_group_devices = sorted(d.name for d in group_path.iterdir())

            # ReBAR
            rebar_path = Path(f"/sys/bus/pci/devices/{pci_address}/resource0")
            if rebar_path.exists():
                size = rebar_path.stat().st_size
                gpu.rebar_supported = size > 256 * 1024 * 1024  # >256MB = ReBAR likely
                gpu.rebar_enabled = size >= gpu.vram_mb * 1024 * 1024 if gpu.vram_mb else False

            # Find audio function (usually .1)
            base = pci_address.rsplit(".", 1)[0]
            audio_addr = f"{base}.1"
            if Path(f"/sys/bus/pci/devices/{audio_addr}").exists():
                gpu.audio_pci = audio_addr

        except Exception as e:
            log.debug("GPU detection failed for %s: %s", pci_address, e)
        return gpu


@dataclass
class VMProfile:
    """A complete VM configuration profile."""
    name: str               # gaming, workstation, server, media
    vmid: int = 0
    vm_name: str = ""

    # Resources
    cores: int = 4
    memory_mb: int = 8192
    hugepages: bool = False
    hugepage_size: str = "2M"  # 2M or 1G

    # CPU
    cpu_pinning: list[int] = field(default_factory=list)
    cpu_model: str = "host"
    numa: bool = False

    # GPU
    gpu_passthrough: bool = False
    gpu_pci: str = ""
    gpu_audio_pci: str = ""
    rebar: bool = False
    virtual_display: bool = False  # IddCx for headless passthrough

    # Display
    display_heads: int = 1
    display_type: str = "dbus"  # dbus, kvmfr, none
    ivshmem_size_mb: int = 64
    looking_glass: bool = False

    # Audio
    audio_channels: int = 2     # 2=stereo, 6=5.1, 8=7.1

    # USB
    usb_controller_passthrough: str = ""  # PCI address of USB controller

    # Network
    network_type: str = "virtio"  # virtio, e1000, passthrough

    @classmethod
    def gaming(cls, vmid: int, gpu_pci: str = "", cores: int = 8,
               memory_mb: int = 16384) -> "VMProfile":
        """Gaming profile — maximum performance."""
        profile = cls(name="gaming", vmid=vmid, cores=cores, memory_mb=memory_mb)

        # CPU: pin cores, host model, enable hugepages
        topo = CPUTopology.detect()
        profile.cpu_pinning = topo.pin_cores(cores)
        profile.hugepages = True
        profile.numa = topo.numa_nodes > 1

        # GPU passthrough
        if gpu_pci:
            gpu = GPUInfo.detect(gpu_pci)
            profile.gpu_passthrough = True
            profile.gpu_pci = gpu_pci
            profile.gpu_audio_pci = gpu.audio_pci
            profile.rebar = gpu.rebar_supported
            profile.virtual_display = True  # IddCx for headless
            profile.looking_glass = True
            profile.ivshmem_size_mb = 128  # 4K needs more
            profile.display_type = "kvmfr"

        # Audio: 5.1 surround for gaming
        profile.audio_channels = 6

        # Display: single head (GPU provides the real displays)
        profile.display_heads = 1

        return profile

    @classmethod
    def workstation(cls, vmid: int, displays: int = 2,
                    cores: int = 4, memory_mb: int = 8192) -> "VMProfile":
        """Workstation profile — balanced multi-monitor."""
        profile = cls(name="workstation", vmid=vmid, cores=cores, memory_mb=memory_mb)
        profile.display_heads = displays
        profile.display_type = "dbus"
        profile.ivshmem_size_mb = 64
        profile.looking_glass = True
        profile.audio_channels = 2
        return profile

    @classmethod
    def server(cls, vmid: int, cores: int = 2, memory_mb: int = 4096) -> "VMProfile":
        """Server profile — minimal, headless."""
        profile = cls(name="server", vmid=vmid, cores=cores, memory_mb=memory_mb)
        profile.display_heads = 1
        profile.display_type = "dbus"  # still need display for BIOS/console
        profile.ivshmem_size_mb = 32
        profile.audio_channels = 0
        return profile

    @classmethod
    def media(cls, vmid: int, cores: int = 4, memory_mb: int = 8192) -> "VMProfile":
        """Media profile — surround audio, hardware decode."""
        profile = cls(name="media", vmid=vmid, cores=cores, memory_mb=memory_mb)
        profile.display_heads = 1
        profile.display_type = "dbus"
        profile.ivshmem_size_mb = 64
        profile.looking_glass = True
        profile.audio_channels = 8  # 7.1
        return profile

    def qemu_args(self) -> list[str]:
        """Generate QEMU command-line arguments for this profile."""
        args = []
        name = self.vm_name or f"vm{self.vmid}"

        # CPU
        args += ["-cpu", self.cpu_model]
        args += ["-smp", f"{self.cores},sockets=1,cores={self.cores},threads=1"]

        # Memory
        if self.hugepages:
            args += ["-m", str(self.memory_mb),
                     "-mem-prealloc", "-mem-path", "/dev/hugepages"]
        else:
            args += ["-m", str(self.memory_mb)]

        # Display
        if self.display_type == "dbus":
            args += ["-display", "dbus"]
        elif self.display_type == "kvmfr":
            # Secondary virtio-vga driven via D-Bus for management console.
            # The passed-through GPU owns the real monitors; this virtual display
            # gives the ozma display service a separate management console path
            # (BIOS, OS install, agent control) without touching the gaming GPU.
            args += ["-display", "dbus,p2p=yes"]
        else:
            args += ["-display", "none"]

        # Virtio-GPU (for non-passthrough display)
        if not self.gpu_passthrough:
            if self.display_heads > 1:
                args += ["-device", f"virtio-gpu-pci,id=vga0,max_outputs={self.display_heads}"]
            else:
                args += ["-device", "virtio-vga"]
        elif self.display_type == "kvmfr":
            # Secondary virtio-vga alongside passed-through GPU.
            # Proxmox needs vga: virtio in the conf to keep this alive.
            args += ["-device", "virtio-vga,id=vga0"]

        # GPU passthrough
        if self.gpu_passthrough and self.gpu_pci:
            gpu_args = f"vfio-pci,host={self.gpu_pci},id=gpu0,multifunction=on"
            if self.rebar:
                gpu_args += ",x-pci-vendor-id=0x10de"  # NVIDIA specific
            args += ["-device", gpu_args]
            # GPU audio
            if self.gpu_audio_pci:
                args += ["-device", f"vfio-pci,host={self.gpu_audio_pci},id=gpu0-audio"]

        # IVSHMEM (Looking Glass / KVMFR)
        if self.looking_glass or self.display_type == "kvmfr":
            shm = f"/dev/shm/ozma-vm{self.vmid}"
            args += ["-object", f"memory-backend-file,id=ozma-shm,share=on,mem-path={shm},size={self.ivshmem_size_mb}M"]
            args += ["-device", "ivshmem-plain,memdev=ozma-shm"]

        # Audio — single multi-channel PipeWire sink
        if self.audio_channels > 0:
            args += ["-audiodev", f"pipewire,id=ozma-audio,out.name=ozma-{name},out.channels={self.audio_channels}"]
            args += ["-device", "intel-hda", "-device", "hda-duplex,audiodev=ozma-audio"]

        # USB
        args += ["-device", "qemu-xhci,id=xhci"]
        args += ["-device", "usb-tablet,bus=xhci.0"]

        # USB controller passthrough (gaming: dedicated controller for peripherals)
        if self.usb_controller_passthrough:
            args += ["-device", f"vfio-pci,host={self.usb_controller_passthrough},id=usb-passthrough"]

        # QMP
        args += ["-qmp", f"unix:/var/run/ozma/vm{self.vmid}-ctrl.qmp,server,nowait"]

        # Network
        if self.network_type == "virtio":
            args += ["-device", "virtio-net-pci,netdev=net0"]
            args += ["-netdev", "user,id=net0"]

        return args

    def cpu_pinning_args(self) -> str:
        """Generate taskset/cgroup CPU pinning commands."""
        if not self.cpu_pinning:
            return ""
        cpus = ",".join(str(c) for c in self.cpu_pinning)
        return f"taskset -c {cpus}"

    def host_setup_commands(self) -> list[str]:
        """Commands to run on the host before starting the VM."""
        cmds = []

        # Hugepages
        if self.hugepages:
            pages_needed = self.memory_mb // 2  # 2MB pages
            cmds.append(f"echo {pages_needed} > /proc/sys/vm/nr_hugepages")

        # IVSHMEM shared memory
        if self.looking_glass or self.display_type == "kvmfr":
            shm = f"/dev/shm/ozma-vm{self.vmid}"
            cmds.append(f"truncate -s {self.ivshmem_size_mb}M {shm}")
            cmds.append(f"chmod 666 {shm}")

        # VFIO driver binding for GPU
        if self.gpu_passthrough and self.gpu_pci:
            cmds.append(f"# Bind {self.gpu_pci} to vfio-pci")
            cmds.append(f"echo 'vfio-pci' > /sys/bus/pci/devices/{self.gpu_pci}/driver_override")
            cmds.append(f"echo '{self.gpu_pci}' > /sys/bus/pci/drivers_probe")

        return cmds

    def proxmox_conf_lines(self) -> list[str]:
        """Generate Proxmox VM config lines."""
        lines = []
        lines.append(f"# Ozma profile: {self.name}")
        lines.append(f"cores: {self.cores}")
        lines.append(f"memory: {self.memory_mb}")
        lines.append(f"cpu: {self.cpu_model}")

        if self.hugepages:
            lines.append("hugepages: 1024")  # Proxmox hugepages option

        if self.gpu_passthrough and self.gpu_pci:
            lines.append(f"hostpci0: {self.gpu_pci},pcie=1,x-vga=1")
            # Keep secondary virtio-vga alive alongside the passed-through GPU.
            # Without this Proxmox removes the virtual display entirely.
            lines.append("vga: virtio")
            if self.gpu_audio_pci:
                lines.append(f"hostpci1: {self.gpu_audio_pci},pcie=1")

        if self.cpu_pinning:
            cpus = ",".join(str(c) for c in self.cpu_pinning)
            lines.append(f"# CPU pinning: taskset -c {cpus}")
            lines.append(f"affinity: {cpus}")

        return lines

    def to_dict(self) -> dict:
        return {
            "name": self.name, "vmid": self.vmid,
            "cores": self.cores, "memory_mb": self.memory_mb,
            "gpu_passthrough": self.gpu_passthrough, "gpu_pci": self.gpu_pci,
            "rebar": self.rebar, "display_heads": self.display_heads,
            "display_type": self.display_type, "audio_channels": self.audio_channels,
            "hugepages": self.hugepages, "cpu_pinning": self.cpu_pinning,
            "looking_glass": self.looking_glass,
        }
