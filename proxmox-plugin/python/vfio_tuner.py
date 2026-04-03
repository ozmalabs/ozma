#!/usr/bin/python3
# SPDX-License-Identifier: GPL-2.0-or-later
"""
VFIO tuning automation — make GPU passthrough gaming VMs plug-and-play.

The hard VFIO tuning (driver binding, hugepages, IVSHMEM, MSI interrupts,
CPU pinning, IOMMU group validation) should be automatic.  This module:

  1. Probes the host — IOMMU state, GPU candidates, driver bindings,
     hugepage availability, VFIO module status, CPU topology
  2. Validates — checks IOMMU group cleanliness, driver conflicts,
     kernel parameter requirements
  3. Plans — generates the complete ordered list of changes needed
  4. Applies — executes changes (requires root; dry-run by default)

Usage:
  # Detect and print full status + plan
  python3 vfio_tuner.py

  # Apply all recommended changes (requires root)
  sudo python3 vfio_tuner.py --apply

  # Apply for a specific GPU
  sudo python3 vfio_tuner.py --apply --gpu 0000:29:00.0

Integration:
  from vfio_tuner import VFIOTuner, VFIOStatus
  status = VFIOTuner.probe()
  print(status.summary())
  if not status.ready:
      for step in status.plan:
          print(step.description)
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path
from typing import Any

_SYS_PCI = Path("/sys/bus/pci/devices")
_SYS_IOMMU = Path("/sys/kernel/iommu_groups")
_SYS_CPU = Path("/sys/devices/system/cpu")
_MODPROBE_DIR = Path("/etc/modprobe.d")
_GRUB_DEFAULT = Path("/etc/default/grub")
_PROC_CMDLINE = Path("/proc/cmdline")

# ── Enums ─────────────────────────────────────────────────────────────────────

class StepKind(Enum):
    MODPROBE    = "modprobe"        # load kernel module
    BIND        = "bind"            # bind device to vfio-pci
    UNBIND      = "unbind"          # unbind device from current driver
    HUGEPAGES   = "hugepages"       # allocate hugepages
    IVSHMEM     = "ivshmem"         # create IVSHMEM shared memory
    MSI         = "msi"             # enable MSI interrupts
    WRITE_CONF  = "write_conf"      # write config file (modprobe.d, etc.)
    GRUB        = "grub"            # update kernel cmdline via GRUB
    INFO        = "info"            # informational — no action needed


# ── Data model ────────────────────────────────────────────────────────────────

@dataclass
class PCIDevice:
    address: str        # e.g. "0000:29:00.0"
    vendor_id: str = ""
    device_id: str = ""
    description: str = ""
    driver: str = ""    # current driver, "" if none
    iommu_group: int = -1
    pci_class: str = ""  # e.g. "0300" = VGA

    @property
    def is_gpu(self) -> bool:
        return self.pci_class.startswith("03")

    @property
    def is_vga(self) -> bool:
        return self.pci_class == "0300"

    @property
    def is_audio(self) -> bool:
        return self.pci_class in ("0403", "0401")

    @property
    def is_usb(self) -> bool:
        return self.pci_class.startswith("0c03")

    @property
    def bound_to_vfio(self) -> bool:
        return self.driver == "vfio-pci"

    @property
    def vendor_name(self) -> str:
        if self.vendor_id == "10de":
            return "NVIDIA"
        if self.vendor_id in ("1002", "1022"):
            return "AMD"
        if self.vendor_id == "8086":
            return "Intel"
        return self.vendor_id


@dataclass
class IOMMUGroup:
    group_id: int
    devices: list[PCIDevice] = field(default_factory=list)

    @property
    def is_clean(self) -> bool:
        """A clean group contains only devices from a single GPU (VGA + audio + usb)."""
        vendors = {d.vendor_id for d in self.devices if d.is_gpu or d.is_audio or d.is_usb}
        # Allow PCIe bridges in the group (common on AMD)
        non_bridge = [d for d in self.devices if d.pci_class not in ("0604", "0600")]
        if not non_bridge:
            return True
        vendors = {d.vendor_id for d in non_bridge}
        return len(vendors) == 1

    @property
    def has_bridge(self) -> bool:
        return any(d.pci_class in ("0604", "0600") for d in self.devices)

    @property
    def gpus(self) -> list[PCIDevice]:
        return [d for d in self.devices if d.is_vga]

    @property
    def all_bound_to_vfio(self) -> bool:
        return all(d.bound_to_vfio for d in self.devices
                   if d.pci_class not in ("0604", "0600"))


@dataclass
class GPUCandidate:
    """A GPU suitable for passthrough (not the primary display GPU)."""
    gpu: PCIDevice
    group: IOMMUGroup
    rebar_enabled: bool = False
    rebar_size_mb: int = 0
    vram_mb: int = 0
    is_primary: bool = False   # True if currently driving the host display

    @property
    def passthrough_devices(self) -> list[PCIDevice]:
        """All devices that must be passed through together (whole IOMMU group, minus bridges)."""
        return [d for d in self.group.devices if d.pci_class not in ("0604", "0600")]

    @property
    def ready_for_passthrough(self) -> bool:
        return self.group.all_bound_to_vfio

    def summary(self) -> str:
        status = "READY" if self.ready_for_passthrough else "needs binding"
        primary = " [PRIMARY — host display]" if self.is_primary else ""
        rebar = f" ReBAR={self.rebar_size_mb}MB" if self.rebar_enabled else ""
        return (f"{self.gpu.address} {self.gpu.description[:50]}"
                f" group={self.group.group_id}{rebar} {status}{primary}")


@dataclass
class TuningStep:
    kind: StepKind
    description: str
    command: list[str] = field(default_factory=list)   # shell command to run
    write_path: str = ""                               # file to write
    write_content: str = ""                            # content to write
    requires_reboot: bool = False
    warn: str = ""

    def apply(self, dry_run: bool = True) -> bool:
        """Execute this step. Returns True on success."""
        if dry_run:
            print(f"  [DRY-RUN] {self.description}")
            if self.command:
                print(f"    $ {' '.join(self.command)}")
            if self.write_path:
                print(f"    write {self.write_path}")
            return True

        try:
            if self.write_path and self.write_content is not None:
                path = Path(self.write_path)
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(self.write_content)
                print(f"  ✓ wrote {self.write_path}")

            if self.command:
                result = subprocess.run(self.command, capture_output=True, text=True)
                if result.returncode != 0:
                    print(f"  ✗ {self.description}: {result.stderr.strip()}")
                    return False
                print(f"  ✓ {self.description}")

            return True
        except Exception as exc:
            print(f"  ✗ {self.description}: {exc}")
            return False


@dataclass
class VFIOStatus:
    """Complete VFIO readiness status for this host."""
    iommu_active: bool = False
    iommu_mode: str = ""                    # DMA, DMA-FQ, IDENTITY, NONE
    iommu_pt_mode: bool = False             # iommu=pt in cmdline (recommended)
    vfio_pci_loaded: bool = False
    vfio_iommu_loaded: bool = False
    cpu_model: str = ""
    total_threads: int = 0
    total_cores: int = 0
    hugepages_2m: int = 0
    hugepages_1g: int = 0
    candidates: list[GPUCandidate] = field(default_factory=list)
    plan: list[TuningStep] = field(default_factory=list)
    warnings: list[str] = field(default_factory=list)

    @property
    def ready(self) -> bool:
        """True if at least one non-primary GPU is ready for passthrough."""
        return any(c.ready_for_passthrough and not c.is_primary for c in self.candidates)

    @property
    def passthrough_candidates(self) -> list[GPUCandidate]:
        return [c for c in self.candidates if not c.is_primary]

    def summary(self) -> str:
        lines = ["── VFIO Status ──────────────────────────────────────────"]
        lines.append(f"  IOMMU: {'active' if self.iommu_active else 'INACTIVE'}"
                     f" mode={self.iommu_mode}"
                     f" pt={'yes' if self.iommu_pt_mode else 'no (recommended: add iommu=pt)'}")
        lines.append(f"  CPU:   {self.cpu_model} ({self.total_cores}c/{self.total_threads}t)")
        lines.append(f"  vfio-pci module: {'loaded' if self.vfio_pci_loaded else 'not loaded'}")
        lines.append(f"  Hugepages: 2M={self.hugepages_2m} 1G={self.hugepages_1g}")
        lines.append("")
        lines.append("  GPU candidates:")
        if not self.candidates:
            lines.append("    (none found)")
        for c in self.candidates:
            lines.append(f"    {'*' if c.is_primary else ' '} {c.summary()}")
            for d in c.passthrough_devices:
                bound = f"→ {d.driver}" if d.driver else "→ (unbound)"
                lines.append(f"      {d.address} [{d.pci_class}] {d.description[:40]} {bound}")
        if self.warnings:
            lines.append("")
            lines.append("  Warnings:")
            for w in self.warnings:
                lines.append(f"    ⚠ {w}")
        if self.plan:
            lines.append("")
            lines.append(f"  Plan: {len(self.plan)} step(s) needed")
            for step in self.plan:
                reboot = " [requires reboot]" if step.requires_reboot else ""
                lines.append(f"    [{step.kind.value}] {step.description}{reboot}")
        else:
            lines.append("")
            lines.append("  ✓ No changes needed — ready for passthrough")
        return "\n".join(lines)


# ── Detection ─────────────────────────────────────────────────────────────────

def _read(path: str | Path, default: str = "") -> str:
    try:
        return Path(path).read_text().strip()
    except Exception:
        return default


def _lspci_devices() -> list[PCIDevice]:
    """Parse lspci -mm -n output into PCIDevice list."""
    devices = []
    try:
        out = subprocess.check_output(
            ["lspci", "-mm", "-n"], text=True, stderr=subprocess.DEVNULL
        )
        for line in out.splitlines():
            # Format: "29:00.0" "0300" "10de" "21c4" ...
            parts = re.findall(r'"([^"]*)"', line)
            addr_raw = line.split()[0]
            # Normalise to 0000:xx:xx.x
            if addr_raw.count(":") == 1:
                addr_raw = f"0000:{addr_raw}"
            if len(parts) >= 3:
                devices.append(PCIDevice(
                    address=addr_raw,
                    pci_class=parts[0].replace(" ", ""),
                    vendor_id=parts[1].lower(),
                    device_id=parts[2].lower() if len(parts) > 2 else "",
                ))
    except Exception:
        pass

    # Enrich with human-readable descriptions
    try:
        out = subprocess.check_output(
            ["lspci", "-mm"], text=True, stderr=subprocess.DEVNULL
        )
        for line in out.splitlines():
            addr_raw = line.split()[0]
            if addr_raw.count(":") == 1:
                addr_raw = f"0000:{addr_raw}"
            parts = re.findall(r'"([^"]*)"', line)
            if len(parts) >= 3:
                desc = parts[2]
                if len(parts) >= 4:
                    desc = f"{parts[1]} {parts[2]} {parts[3]}"
                for d in devices:
                    if d.address == addr_raw:
                        d.description = desc.strip()
    except Exception:
        pass

    return devices


def _get_driver(address: str) -> str:
    driver_link = _SYS_PCI / address / "driver"
    if driver_link.is_symlink():
        return Path(os.readlink(str(driver_link))).name
    return ""


def _get_iommu_group(address: str) -> int:
    link = _SYS_PCI / address / "iommu_group"
    if link.is_symlink():
        return int(Path(os.readlink(str(link))).name)
    return -1


def _bar0_size(address: str) -> int:
    try:
        return (_SYS_PCI / address / "resource0").stat().st_size
    except Exception:
        return 0


def _vram_mb(address: str, vendor_id: str) -> int:
    """Estimate VRAM from BAR1 (the main VRAM aperture) in the resource file."""
    try:
        lines = (_SYS_PCI / address / "resource").read_text().splitlines()
        # BAR1 is usually the largest 64-bit prefetchable region
        largest = 0
        for line in lines:
            parts = line.split()
            if len(parts) == 3:
                flags = int(parts[2], 16)
                if flags & 0x8:  # prefetchable
                    start, end = int(parts[0], 16), int(parts[1], 16)
                    if end > start:
                        size = (end - start + 1) // (1024 * 1024)
                        largest = max(largest, size)
        return largest
    except Exception:
        return 0


def _active_drm_cards() -> set[str]:
    """
    Return the set of DRM device names (e.g. "card0") that are actively
    opened by a running compositor (X11, Wayland, GDM, SDDM, etc.).
    """
    active: set[str] = set()
    try:
        # Walk /proc/*/fd looking for open DRM device files
        for proc_fd in Path("/proc").glob("*/fd"):
            try:
                for fd in proc_fd.iterdir():
                    try:
                        target = os.readlink(str(fd))
                        if "/dev/dri/card" in target:
                            active.add(Path(target).name)
                    except OSError:
                        pass
            except PermissionError:
                pass
    except Exception:
        pass
    return active


def _connected_outputs(address: str) -> list[str]:
    """Return list of connector names that have a display physically connected."""
    drm_dir = _SYS_PCI / address / "drm"
    if not drm_dir.exists():
        return []
    connected = []
    for card_dir in drm_dir.iterdir():
        if not card_dir.name.startswith("card"):
            continue
        for conn in Path("/sys/class/drm").glob(f"{card_dir.name}-*"):
            status = _read(conn / "status", "")
            if status == "connected":
                connected.append(conn.name)
    return connected


def _is_primary_gpu(address: str) -> bool:
    """
    Check if this GPU is the primary display GPU (driving the active host session).

    Priority order:
    1. Has physically connected display outputs → definitely primary
    2. Compositor/display-server process has its DRM card open AND connectors
       exist (covers Wayland/X11 with displays connected but status not readable)
    3. boot_vga=1 AND no other GPU has connected outputs → conservatively primary
    4. Headless / no connected outputs on any GPU → not primary (safe to pass through)
    """
    # 1. Physical outputs connected
    if _connected_outputs(address):
        return True

    # 2. Compositor has the card open (only meaningful if card exists)
    drm_dir = _SYS_PCI / address / "drm"
    if drm_dir.exists():
        drm_cards = {d.name for d in drm_dir.iterdir() if d.name.startswith("card")}
        if drm_cards:
            active = _active_drm_cards()
            # Only consider compositor opens, not driver-internal opens
            # Check if a known display server is holding the fd
            compositor_procs = _compositor_pids()
            if active & drm_cards and compositor_procs:
                return True

    # 3. boot_vga=1, but only if this machine has connected outputs elsewhere
    # (if no GPU has connected outputs, we're headless and nothing is primary)
    boot_vga = _read(_SYS_PCI / address / "boot_vga", "0").strip()
    if boot_vga == "1":
        # Check if any GPU has connected outputs — if none do, system is headless
        # and this flag alone doesn't make it non-passthrough-able
        for dev_path in _SYS_PCI.iterdir():
            if dev_path.name == address:
                continue
            if _connected_outputs(dev_path.name):
                return True  # another GPU has displays; boot_vga GPU is spare
            # If this is the only GPU with boot_vga, and no other has outputs,
            # passing it through would break the BIOS/EFI console — flag it
        # No other GPU has outputs — headless system, don't flag as primary
        return False

    return False


def _compositor_pids() -> set[int]:
    """Return PIDs of known display server / compositor processes."""
    compositors = {"Xorg", "X", "gnome-shell", "kwin_wayland", "kwin_x11",
                   "sway", "weston", "mutter", "openbox", "i3", "sddm", "gdm",
                   "lightdm", "labwc", "hyprland", "wayfire", "plasmashell"}
    pids: set[int] = set()
    try:
        for proc in Path("/proc").iterdir():
            if not proc.name.isdigit():
                continue
            comm = _read(proc / "comm", "")
            if comm in compositors:
                pids.add(int(proc.name))
    except Exception:
        pass
    return pids


def _kernel_modules() -> set[str]:
    try:
        out = subprocess.check_output(["lsmod"], text=True)
        return {line.split()[0] for line in out.splitlines()[1:] if line.strip()}
    except Exception:
        return set()


def _hugepages() -> tuple[int, int]:
    """Return (nr_hugepages_2M, nr_hugepages_1G)."""
    h2m = int(_read("/proc/sys/vm/nr_hugepages", "0"))
    h1g = 0
    for path in Path("/sys/devices/system/node").glob("node*/hugepages/hugepages-1048576kB/nr_hugepages"):
        try:
            h1g += int(path.read_text().strip())
        except Exception:
            pass
    return h2m, h1g


# ── Planning ──────────────────────────────────────────────────────────────────

def _plan_for_candidate(
    candidate: GPUCandidate,
    modules: set[str],
    iommu_pt: bool,
) -> list[TuningStep]:
    """Generate tuning steps for a specific GPU candidate."""
    steps = []

    # 1. iommu=pt kernel parameter (reduces overhead, prevents host from using IOMMU-mapped memory)
    if not iommu_pt:
        steps.append(TuningStep(
            kind=StepKind.GRUB,
            description="Add iommu=pt to kernel cmdline (reduces DMA overhead)",
            requires_reboot=True,
            write_path=str(_GRUB_DEFAULT),
            write_content=None,  # handled specially in apply
            warn="Edit /etc/default/grub: add 'iommu=pt' to GRUB_CMDLINE_LINUX_DEFAULT, then run update-grub",
        ))

    # 2. vfio modules
    if "vfio" not in modules:
        steps.append(TuningStep(
            kind=StepKind.MODPROBE,
            description="Load vfio kernel module",
            command=["modprobe", "vfio"],
        ))
    if "vfio_iommu_type1" not in modules:
        steps.append(TuningStep(
            kind=StepKind.MODPROBE,
            description="Load vfio_iommu_type1 kernel module",
            command=["modprobe", "vfio_iommu_type1"],
        ))
    if "vfio_pci" not in modules:
        steps.append(TuningStep(
            kind=StepKind.MODPROBE,
            description="Load vfio-pci kernel module",
            command=["modprobe", "vfio-pci"],
        ))

    # 3. Write modprobe.d config for persistent vfio-pci binding
    ids = ",".join(f"{d.vendor_id}:{d.device_id}" for d in candidate.passthrough_devices
                   if d.vendor_id and d.device_id)
    if ids:
        conf_path = str(_MODPROBE_DIR / f"ozma-vfio-{candidate.gpu.address.replace(':', '-').replace('.', '-')}.conf")
        conf_content = (
            f"# Ozma VFIO binding — {candidate.gpu.description}\n"
            f"# Generated by vfio_tuner.py\n"
            f"options vfio-pci ids={ids}\n"
            f"softdep {candidate.gpu.vendor_name.lower()} pre: vfio-pci\n"
        )
        steps.append(TuningStep(
            kind=StepKind.WRITE_CONF,
            description=f"Write modprobe.d config for persistent binding (ids={ids})",
            write_path=conf_path,
            write_content=conf_content,
            requires_reboot=True,
        ))

    # 4. Unbind from current driver + bind to vfio-pci (runtime, current boot)
    for dev in candidate.passthrough_devices:
        if dev.driver and dev.driver != "vfio-pci":
            steps.append(TuningStep(
                kind=StepKind.UNBIND,
                description=f"Unbind {dev.address} from {dev.driver}",
                command=["sh", "-c",
                    f"echo '{dev.address}' > /sys/bus/pci/drivers/{dev.driver}/unbind"],
            ))
        if dev.driver != "vfio-pci":
            steps.append(TuningStep(
                kind=StepKind.BIND,
                description=f"Bind {dev.address} to vfio-pci",
                command=["sh", "-c",
                    f"echo 'vfio-pci' > /sys/bus/pci/devices/{dev.address}/driver_override"
                    f" && echo '{dev.address}' > /sys/bus/pci/drivers_probe"],
            ))

    return steps


def _plan_hugepages(memory_mb: int, current_2m: int) -> list[TuningStep]:
    needed = memory_mb // 2  # 2M pages
    if current_2m >= needed:
        return []
    return [TuningStep(
        kind=StepKind.HUGEPAGES,
        description=f"Allocate {needed} × 2M hugepages ({memory_mb} MB total) for VM memory",
        command=["sh", "-c", f"echo {needed} > /proc/sys/vm/nr_hugepages"],
    )]


def _plan_ivshmem(vmid: int, size_mb: int = 64) -> list[TuningStep]:
    path = f"/dev/shm/ozma-vm{vmid}"
    if Path(path).exists():
        return [TuningStep(
            kind=StepKind.INFO,
            description=f"IVSHMEM {path} already exists",
        )]
    return [TuningStep(
        kind=StepKind.IVSHMEM,
        description=f"Create IVSHMEM for Looking Glass ({path}, {size_mb} MB)",
        command=["sh", "-c", f"truncate -s {size_mb}M {path} && chmod 666 {path}"],
    )]


# ── Main probe ────────────────────────────────────────────────────────────────

class VFIOTuner:

    @staticmethod
    def probe(target_gpu: str = "") -> VFIOStatus:
        """
        Probe the system and return a complete VFIOStatus.

        Args:
            target_gpu: specific PCI address to focus on (e.g. "0000:29:00.0").
                        If empty, auto-detects all non-primary GPUs.
        """
        status = VFIOStatus()

        # ── CPU ──
        try:
            out = subprocess.check_output(["lscpu", "-J"], text=True)
            data = json.loads(out)
            for item in data.get("lscpu", []):
                f = item.get("field", "").rstrip(":")
                v = item.get("data", "")
                if f == "Model name":
                    status.cpu_model = v
                elif f == "CPU(s)":
                    status.total_threads = int(v)
                elif f == "Core(s) per socket":
                    cores_per_socket = int(v)
                elif f == "Socket(s)":
                    sockets = int(v)
            status.total_cores = cores_per_socket * sockets
        except Exception:
            status.cpu_model = _read("/proc/cpuinfo").split("model name")[1].split("\n")[0].lstrip(": ") if "model name" in _read("/proc/cpuinfo") else "unknown"

        # ── IOMMU ──
        cmdline = _read(_PROC_CMDLINE)
        status.iommu_pt_mode = "iommu=pt" in cmdline

        # If IOMMU groups exist in sysfs, IOMMU is active
        if _SYS_IOMMU.exists() and list(_SYS_IOMMU.iterdir()):
            status.iommu_active = True
            # Read mode from any group's type file
            for gdir in sorted(_SYS_IOMMU.iterdir())[:5]:
                t = _read(gdir / "type", "")
                if t:
                    status.iommu_mode = t
                    break
        else:
            status.warnings.append(
                "IOMMU groups not found — enable IOMMU in BIOS "
                "and add amd_iommu=on (or intel_iommu=on) to kernel cmdline"
            )

        # ── Modules ──
        modules = _kernel_modules()
        status.vfio_pci_loaded = "vfio_pci" in modules
        status.vfio_iommu_loaded = "vfio_iommu_type1" in modules

        # ── Hugepages ──
        status.hugepages_2m, status.hugepages_1g = _hugepages()

        # ── PCI devices ──
        all_devices = _lspci_devices()
        for dev in all_devices:
            dev.driver = _get_driver(dev.address)
            dev.iommu_group = _get_iommu_group(dev.address)

        # Build IOMMU group map
        groups: dict[int, IOMMUGroup] = {}
        for dev in all_devices:
            if dev.iommu_group >= 0:
                grp = groups.setdefault(dev.iommu_group, IOMMUGroup(dev.iommu_group))
                grp.devices.append(dev)

        # ── GPU candidates ──
        seen_groups: set[int] = set()
        for dev in all_devices:
            if not dev.is_vga:
                continue
            if dev.iommu_group in seen_groups:
                continue
            seen_groups.add(dev.iommu_group)

            if target_gpu and dev.address != target_gpu:
                continue

            grp = groups.get(dev.iommu_group, IOMMUGroup(dev.iommu_group, [dev]))
            bar0 = _bar0_size(dev.address)
            vram = _vram_mb(dev.address, dev.vendor_id)

            candidate = GPUCandidate(
                gpu=dev,
                group=grp,
                rebar_enabled=bar0 > 256 * 1024 * 1024,
                rebar_size_mb=bar0 // (1024 * 1024) if bar0 > 256 * 1024 * 1024 else 0,
                vram_mb=vram,
                is_primary=_is_primary_gpu(dev.address),
            )

            if not grp.is_clean:
                status.warnings.append(
                    f"IOMMU group {dev.iommu_group} is not clean — "
                    f"contains devices from multiple vendors. "
                    f"You may need the ACS override patch."
                )

            status.candidates.append(candidate)

        # ── Build plan ──
        non_primary = [c for c in status.candidates
                       if not c.is_primary and not c.ready_for_passthrough]

        if target_gpu:
            # Explicit target: plan only for that GPU
            plan_candidates = [c for c in non_primary if c.gpu.address == target_gpu]
            if not plan_candidates:
                status.warnings.append(
                    f"Specified GPU {target_gpu} not found or already ready"
                )
        elif len(non_primary) == 1:
            # Only one candidate — unambiguous
            plan_candidates = non_primary
        elif len(non_primary) > 1:
            # Multiple candidates, no explicit target — don't guess
            plan_candidates = []
            addrs = ", ".join(c.gpu.address for c in non_primary)
            status.warnings.append(
                f"Multiple passthrough candidates found ({addrs}). "
                f"Specify one with --gpu <address> to generate a binding plan."
            )
        else:
            plan_candidates = []

        for candidate in plan_candidates:
            steps = _plan_for_candidate(candidate, modules, status.iommu_pt_mode)
            status.plan.extend(steps)

        # Deduplicate modprobe steps
        seen_cmds: set[str] = set()
        deduped = []
        for step in status.plan:
            key = " ".join(step.command) if step.command else step.description
            if key not in seen_cmds:
                seen_cmds.add(key)
                deduped.append(step)
        status.plan = deduped

        return status

    @staticmethod
    def apply(status: VFIOStatus, dry_run: bool = True, vmid: int = 100, memory_mb: int = 8192) -> bool:
        """
        Execute the tuning plan.

        Args:
            status:     from VFIOTuner.probe()
            dry_run:    if True, print commands without running them (default)
            vmid:       VM ID for IVSHMEM path
            memory_mb:  VM memory for hugepage allocation
        """
        if not status.iommu_active:
            print("ERROR: IOMMU not active — enable in BIOS and add amd_iommu=on to kernel cmdline")
            return False

        if os.geteuid() != 0 and not dry_run:
            print("ERROR: must run as root to apply changes")
            return False

        all_steps = list(status.plan)
        all_steps += _plan_hugepages(memory_mb, status.hugepages_2m)
        all_steps += _plan_ivshmem(vmid)

        needs_reboot = any(s.requires_reboot for s in all_steps)
        reboot_steps = [s for s in all_steps if s.requires_reboot]
        runtime_steps = [s for s in all_steps if not s.requires_reboot]

        print(f"\n{'DRY RUN — ' if dry_run else ''}Applying {len(all_steps)} tuning step(s):\n")

        for step in runtime_steps:
            if step.warn:
                print(f"  ⚠ {step.warn}")
            step.apply(dry_run=dry_run)

        if reboot_steps:
            print(f"\nThe following steps require a reboot:")
            for step in reboot_steps:
                print(f"  • {step.description}")
                if step.warn:
                    print(f"    ⚠ {step.warn}")
                if not dry_run and step.write_path and step.write_content:
                    step.apply(dry_run=False)

        if needs_reboot:
            print("\n⚠ Reboot required for some changes to take effect.")

        return True


# ── CLI ───────────────────────────────────────────────────────────────────────

if __name__ == "__main__":
    import argparse
    parser = argparse.ArgumentParser(description="Ozma VFIO tuning automation")
    parser.add_argument("--apply", action="store_true", help="Apply changes (requires root)")
    parser.add_argument("--gpu", default="", help="Target GPU PCI address (e.g. 0000:29:00.0)")
    parser.add_argument("--vmid", type=int, default=100, help="VM ID for IVSHMEM")
    parser.add_argument("--memory", type=int, default=8192, help="VM memory MB for hugepages")
    args = parser.parse_args()

    status = VFIOTuner.probe(target_gpu=args.gpu)
    print(status.summary())

    if args.apply or status.plan:
        print()
        VFIOTuner.apply(
            status,
            dry_run=not args.apply,
            vmid=args.vmid,
            memory_mb=args.memory,
        )
