# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
#!/usr/bin/env python3
"""
Ozma Hardware Info — HWiNFO64-parity hardware enumeration and sensor monitoring.

Runs inside the target machine (via ozma agent) and provides:

  1. Full hardware tree: CPU topology + cache + instructions, memory SPD + XMP,
     GPU clocks/power/fan/bandwidth, storage SMART + NVMe wear, motherboard
     VRM/PCH, PCIe topology, battery health, all voltage rails, all fan RPMs.

  2. Real-time sensor polling at 1 Hz (configurable): per-core temperatures,
     per-core clocks, RAPL package/core/uncore power, GPU hot-spot, VRAM temp,
     memory bandwidth, per-disk read/write rates.

  3. Full SMART attribute table (ID, name, value, worst, raw, threshold).

  4. Report generation: JSON, text, CSV, HTML — equivalent to HWiNFO64 summary.

  5. Prometheus metrics integration: extend prometheus_metrics.py with deep
     hardware sensors as additional ozma_node_* gauge metrics.

Cross-platform:
  - Linux:   sysfs (hwmon, coretemp, k10temp, RAPL), dmidecode, lscpu,
             smartctl, nvme-cli, lspci, /sys/class/power_supply/
  - Windows: WMI (Win32_*) + LibreHardwareMonitor WMI backend (root\\LibreHardwareMonitor)
             for deep sensor access (voltages, per-core temps, fan curves)
  - macOS:   sysctl, system_profiler, IOKit, powermetrics (root for power/fan)

Windows deep sensors:
  LibreHardwareMonitor (LHM) is an open-source WMI sensor provider.
  If not present, the agent ships a bundled LHM server and starts it on demand.
  LHM provides all sensor classes: Temperature, Voltage, Fan, Power, Clock, Load,
  Data, SmallData, Throughput, Control — the same set HWiNFO64 reads.
"""

from __future__ import annotations

import asyncio
import csv
import io
import json
import os
import platform
import re
import subprocess
import time
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Any

log = __import__("logging").getLogger("ozma.hardware_info")


# ── Data classes ───────────────────────────────────────────────────────────────

@dataclass
class CacheInfo:
    level: int          # 1, 2, 3
    size_kb: int
    type: str           # Data, Instruction, Unified
    ways: int = 0
    sets: int = 0


@dataclass
class CpuCoreInfo:
    index: int
    temperature_c: float = 0.0
    clock_mhz: float = 0.0
    voltage_v: float = 0.0
    usage_percent: float = 0.0
    power_w: float = 0.0


@dataclass
class CpuPackageInfo:
    brand: str = ""
    model: str = ""
    socket: str = ""
    sockets: int = 1
    physical_cores: int = 0
    logical_cores: int = 0
    base_clock_mhz: float = 0.0
    boost_clock_mhz: float = 0.0
    tdp_w: float = 0.0
    microcode: str = ""
    # Sensors
    package_temp_c: float = 0.0
    package_power_w: float = 0.0
    core_power_w: float = 0.0
    uncore_power_w: float = 0.0
    dram_power_w: float = 0.0
    core_voltage_v: float = 0.0
    # Instruction sets
    instructions: list[str] = field(default_factory=list)
    # Cache hierarchy
    caches: list[CacheInfo] = field(default_factory=list)
    # Per-core data
    cores: list[CpuCoreInfo] = field(default_factory=list)


@dataclass
class MemoryTiming:
    cl: int = 0         # CAS latency
    trcd: int = 0
    trp: int = 0
    tras: int = 0
    trc: int = 0
    voltage_v: float = 0.0


@dataclass
class MemoryXmpProfile:
    number: int = 0
    speed_mhz: int = 0
    timing: MemoryTiming = field(default_factory=MemoryTiming)
    voltage_v: float = 0.0


@dataclass
class MemorySlotInfo:
    slot: str = ""
    size_gb: float = 0.0
    type: str = ""          # DDR4, DDR5, LPDDR5, etc.
    speed_mhz: int = 0
    configured_speed_mhz: int = 0
    manufacturer: str = ""
    part_number: str = ""
    serial_number: str = ""
    form_factor: str = ""   # DIMM, SO-DIMM, etc.
    rank: str = ""
    bank: str = ""
    channel: str = ""
    timing: MemoryTiming = field(default_factory=MemoryTiming)
    xmp_profiles: list[MemoryXmpProfile] = field(default_factory=list)
    # Live sensors (if available)
    temperature_c: float = 0.0
    bandwidth_read_gbs: float = 0.0
    bandwidth_write_gbs: float = 0.0


@dataclass
class GpuInfo:
    index: int = 0
    vendor: str = ""        # NVIDIA, AMD, Intel
    model: str = ""
    vram_gb: float = 0.0
    pcie_slot: int = -1
    pcie_width: int = 0     # x4, x8, x16
    pcie_gen: int = 0       # 3, 4, 5
    driver_version: str = ""
    bios_version: str = ""
    cuda_cores: int = 0
    shader_processors: int = 0
    rops: int = 0
    tmus: int = 0
    # Live sensors
    temperature_c: float = 0.0
    hotspot_temp_c: float = 0.0
    vram_temp_c: float = 0.0
    core_clock_mhz: float = 0.0
    memory_clock_mhz: float = 0.0
    shader_clock_mhz: float = 0.0
    power_w: float = 0.0
    power_limit_w: float = 0.0
    fan_rpm: int = 0
    fan_percent: float = 0.0
    utilization_percent: float = 0.0
    vram_used_gb: float = 0.0
    memory_bandwidth_gbs: float = 0.0
    nvenc_usage_percent: float = 0.0
    nvdec_usage_percent: float = 0.0
    pcie_bandwidth_mbs: float = 0.0


@dataclass
class SmartAttribute:
    id: int
    name: str
    value: int          # normalized 0-200
    worst: int
    raw: int
    threshold: int
    flags: int = 0
    # Pre-fail flag means this attribute predicts failure if below threshold
    pre_fail: bool = False
    # Whether this attribute is currently failing
    failing: bool = False


@dataclass
class NvmeData:
    temperature_c: float = 0.0
    available_spare_percent: int = 0
    available_spare_threshold_percent: int = 0
    percentage_used: int = 0         # wear level (0 = new, 100 = worn)
    data_units_written: int = 0      # in 512KiB units
    data_units_read: int = 0
    host_write_commands: int = 0
    host_read_commands: int = 0
    media_errors: int = 0
    num_err_log_entries: int = 0
    power_on_hours: int = 0
    unsafe_shutdowns: int = 0
    critical_warning: int = 0        # bitmask; 0 = healthy


@dataclass
class SmartData:
    health_percent: int = 100
    overall_status: str = "PASSED"   # PASSED, FAILED, OLD_AGE, PRE-FAIL
    power_on_hours: int = 0
    start_stop_count: int = 0
    reallocated_sectors: int = 0
    pending_sectors: int = 0
    uncorrectable_errors: int = 0
    temperature_c: float = 0.0
    attributes: list[SmartAttribute] = field(default_factory=list)
    self_test_result: str = ""
    # NVMe-specific (populated for NVMe devices)
    nvme: NvmeData | None = None


@dataclass
class StorageInfo:
    device: str = ""        # /dev/sda, /dev/nvme0n1, \\.\PhysicalDrive0
    model: str = ""
    serial: str = ""
    firmware: str = ""
    interface: str = ""     # NVMe, SATA, SAS, USB
    form_factor: str = ""   # 2.5", 3.5", M.2, U.2
    capacity_bytes: int = 0
    rotational: bool = False
    rpm: int = 0
    # Live sensors
    temperature_c: float = 0.0
    read_rate_mbs: float = 0.0
    write_rate_mbs: float = 0.0
    # Full SMART
    smart: SmartData | None = None


@dataclass
class MotherboardInfo:
    manufacturer: str = ""
    model: str = ""
    version: str = ""
    bios_vendor: str = ""
    bios_version: str = ""
    bios_date: str = ""
    chipset: str = ""
    form_factor: str = ""
    # Live sensors
    vrm_temp_c: float = 0.0
    pch_temp_c: float = 0.0
    ambient_temp_c: float = 0.0


@dataclass
class FanReading:
    name: str
    rpm: int
    percent: float = 0.0
    target_rpm: int = 0
    controllable: bool = False


@dataclass
class VoltageReading:
    name: str
    voltage_v: float
    min_v: float = 0.0
    max_v: float = 0.0
    nominal_v: float = 0.0


@dataclass
class BatteryInfo:
    name: str = ""
    status: str = ""        # Charging, Discharging, Full, Not charging
    capacity_design_mwh: int = 0
    capacity_full_mwh: int = 0
    capacity_now_mwh: int = 0
    charge_rate_mw: int = 0
    voltage_mv: int = 0
    temperature_c: float = 0.0
    cycle_count: int = 0
    technology: str = ""    # Li-ion, LiPo, etc.

    @property
    def health_percent(self) -> int:
        if self.capacity_design_mwh > 0:
            return min(100, int(self.capacity_full_mwh * 100 / self.capacity_design_mwh))
        return 0

    @property
    def charge_percent(self) -> float:
        if self.capacity_full_mwh > 0:
            return min(100.0, self.capacity_now_mwh * 100.0 / self.capacity_full_mwh)
        return 0.0


@dataclass
class PcieDevice:
    slot: str = ""
    class_name: str = ""
    vendor: str = ""
    device: str = ""
    subsystem: str = ""
    driver: str = ""
    width: int = 0
    gen: int = 0


@dataclass
class NetworkInfo:
    name: str = ""
    mac: str = ""
    speed_mbps: int = 0
    duplex: str = ""
    driver: str = ""
    link_up: bool = False
    # Live counters
    rx_bytes: int = 0
    tx_bytes: int = 0
    rx_rate_mbs: float = 0.0
    tx_rate_mbs: float = 0.0
    rx_errors: int = 0
    tx_errors: int = 0


@dataclass
class HardwareSnapshot:
    """Full point-in-time hardware inventory + sensor readings."""
    timestamp: float = 0.0
    hostname: str = ""
    os: str = ""
    os_version: str = ""
    cpu: CpuPackageInfo = field(default_factory=CpuPackageInfo)
    memory_slots: list[MemorySlotInfo] = field(default_factory=list)
    gpus: list[GpuInfo] = field(default_factory=list)
    storage: list[StorageInfo] = field(default_factory=list)
    motherboard: MotherboardInfo = field(default_factory=MotherboardInfo)
    fans: list[FanReading] = field(default_factory=list)
    voltages: list[VoltageReading] = field(default_factory=list)
    batteries: list[BatteryInfo] = field(default_factory=list)
    pcie_devices: list[PcieDevice] = field(default_factory=list)
    network: list[NetworkInfo] = field(default_factory=list)
    total_memory_gb: float = 0.0

    def to_dict(self) -> dict:
        return asdict(self)


# ── Linux backend ──────────────────────────────────────────────────────────────

class LinuxHardwareCollector:
    """
    Collect hardware info and sensors on Linux via sysfs, dmidecode,
    lscpu, smartctl, nvme-cli, lspci. No root required for most paths
    (sysfs sensors, cpufreq). dmidecode/smartctl need root for full data
    but degrade gracefully without it.
    """

    def __init__(self) -> None:
        self._prev_net: dict[str, tuple[int, int, float]] = {}
        self._prev_disk: dict[str, tuple[int, int, float]] = {}

    def collect(self) -> HardwareSnapshot:
        snap = HardwareSnapshot(
            timestamp=time.time(),
            hostname=platform.node(),
            os=platform.system(),
            os_version=platform.release(),
        )
        snap.cpu = self._collect_cpu()
        snap.memory_slots = self._collect_memory()
        snap.total_memory_gb = sum(s.size_gb for s in snap.memory_slots) or self._total_ram_gb()
        snap.gpus = self._collect_gpus()
        snap.storage = self._collect_storage()
        snap.motherboard = self._collect_motherboard()
        snap.fans, snap.voltages = self._collect_hwmon_fans_voltages()
        snap.batteries = self._collect_batteries()
        snap.pcie_devices = self._collect_pcie()
        snap.network = self._collect_network()
        return snap

    # ── CPU ──────────────────────────────────────────────────────────────

    def _collect_cpu(self) -> CpuPackageInfo:
        cpu = CpuPackageInfo()

        # Basic info from /proc/cpuinfo
        try:
            cpuinfo = Path("/proc/cpuinfo").read_text()
            for line in cpuinfo.splitlines():
                k, _, v = line.partition(":")
                k = k.strip()
                v = v.strip()
                if k == "model name" and not cpu.brand:
                    cpu.brand = v
                    cpu.model = v
                elif k == "vendor_id" and not cpu.socket:
                    cpu.socket = "AM5" if "AMD" in v else "LGA" if "Intel" in v else v
                elif k == "cpu MHz" and not cpu.base_clock_mhz:
                    try:
                        cpu.base_clock_mhz = float(v)
                    except ValueError:
                        pass
                elif k == "flags" and not cpu.instructions:
                    flags = v.split()
                    known = {"avx", "avx2", "avx512f", "avx512bw", "avx512cd",
                             "avx512dq", "avx512vl", "avx512vnni", "avx512bf16",
                             "amx_bf16", "amx_tile", "sse4_2", "sse4_1", "sse4a",
                             "aes", "pclmulqdq", "sha_ni", "vaes", "vpclmulqdq",
                             "fma", "f16c", "bmi1", "bmi2", "adx", "rdseed",
                             "rdrand", "clmul", "cx16", "movbe", "popcnt",
                             "tsc_deadline_timer", "xsave", "xsavec",
                             "hypervisor", "ept", "vnmi", "x2apic", "lm"}
                    cpu.instructions = sorted(f.upper() for f in flags if f.lower() in known)
            physical_ids = set(re.findall(r"physical id\s*:\s*(\d+)", cpuinfo))
            cpu.sockets = max(1, len(physical_ids))
            cores_list = re.findall(r"cpu cores\s*:\s*(\d+)", cpuinfo)
            if cores_list:
                cpu.physical_cores = int(cores_list[0]) * cpu.sockets
            cpu.logical_cores = os.cpu_count() or 1
        except OSError:
            cpu.logical_cores = os.cpu_count() or 1

        # lscpu for cache hierarchy and more accurate info
        try:
            out = subprocess.run(
                ["lscpu", "--json"], capture_output=True, text=True, timeout=5
            )
            if out.returncode == 0:
                data = json.loads(out.stdout)
                entries = {e["field"].rstrip(":"): e.get("data", "")
                           for e in data.get("lscpu", [])}
                if not cpu.brand:
                    cpu.brand = entries.get("Model name", "")
                    cpu.model = cpu.brand
                for key, level in [("L1d cache", 1), ("L1i cache", 1),
                                    ("L2 cache", 2), ("L3 cache", 3)]:
                    val = entries.get(key, "")
                    m = re.match(r"(\d+(?:\.\d+)?)\s*(K|M|G)", val, re.I)
                    if m:
                        size_kb = float(m.group(1)) * {"K": 1, "M": 1024, "G": 1048576}[m.group(2).upper()]
                        t = "Data" if "d cache" in key else "Instruction" if "i cache" in key else "Unified"
                        cpu.caches.append(CacheInfo(level=level, size_kb=int(size_kb), type=t))
                # Max MHz
                max_mhz = entries.get("CPU max MHz", "")
                if max_mhz:
                    try:
                        cpu.boost_clock_mhz = float(max_mhz)
                    except ValueError:
                        pass
                min_mhz = entries.get("CPU min MHz", "")
                if min_mhz and not cpu.base_clock_mhz:
                    try:
                        cpu.base_clock_mhz = float(min_mhz)
                    except ValueError:
                        pass
        except (OSError, json.JSONDecodeError, KeyError):
            pass

        # Microcode from /proc/cpuinfo
        try:
            for line in Path("/proc/cpuinfo").read_text().splitlines():
                if line.startswith("microcode"):
                    cpu.microcode = line.split(":", 1)[1].strip()
                    break
        except OSError:
            pass

        # Per-core data: temp + clock
        cpu.cores = self._collect_cpu_cores(cpu.logical_cores)

        # Package temperature (coretemp/k10temp hwmon)
        pkg_temp = self._read_hwmon_package_temp()
        if pkg_temp:
            cpu.package_temp_c = pkg_temp
        elif cpu.cores:
            cpu.package_temp_c = max(c.temperature_c for c in cpu.cores if c.temperature_c)

        # RAPL power
        cpu.package_power_w, cpu.core_power_w, cpu.uncore_power_w, cpu.dram_power_w = \
            self._read_rapl_power()

        # VCore voltage from hwmon
        cpu.core_voltage_v = self._read_vcore()

        return cpu

    def _collect_cpu_cores(self, num_cores: int) -> list[CpuCoreInfo]:
        cores = []
        hwmon_temps = self._read_hwmon_core_temps()

        for i in range(num_cores):
            core = CpuCoreInfo(index=i)
            # Clock
            freq_path = Path(f"/sys/devices/system/cpu/cpu{i}/cpufreq/scaling_cur_freq")
            if freq_path.exists():
                try:
                    core.clock_mhz = int(freq_path.read_text().strip()) / 1000.0
                except (ValueError, OSError):
                    pass
            # Temperature
            if i < len(hwmon_temps):
                core.temperature_c = hwmon_temps[i]
            # Usage from /proc/stat (cumulative — snapshot gap needed for live rate)
            cores.append(core)
        return cores

    def _read_hwmon_core_temps(self) -> list[float]:
        """Read per-core temperatures from hwmon (coretemp or k10temp)."""
        temps: dict[int, float] = {}
        hwmon_base = Path("/sys/class/hwmon")
        if not hwmon_base.exists():
            return []
        for hwmon in sorted(hwmon_base.iterdir()):
            try:
                name = (hwmon / "name").read_text().strip()
                if name not in ("coretemp", "k10temp", "zenpower", "nct6775",
                                "nct6779", "w83795g"):
                    continue
                for inp in sorted(hwmon.glob("temp*_input")):
                    label_path = inp.parent / inp.name.replace("input", "label")
                    label = label_path.read_text().strip() if label_path.exists() else ""
                    m = re.search(r"Core\s+(\d+)", label)
                    if m:
                        core_idx = int(m.group(1))
                        try:
                            temps[core_idx] = float(inp.read_text().strip()) / 1000.0
                        except (ValueError, OSError):
                            pass
            except OSError:
                continue
        if not temps:
            return []
        max_idx = max(temps.keys())
        return [temps.get(i, 0.0) for i in range(max_idx + 1)]

    def _read_hwmon_package_temp(self) -> float:
        """Read CPU package temperature (Tdie / Package id 0)."""
        hwmon_base = Path("/sys/class/hwmon")
        if not hwmon_base.exists():
            return 0.0
        for hwmon in sorted(hwmon_base.iterdir()):
            try:
                name = (hwmon / "name").read_text().strip()
                if name not in ("coretemp", "k10temp", "zenpower"):
                    continue
                for inp in sorted(hwmon.glob("temp*_input")):
                    label_path = inp.parent / inp.name.replace("input", "label")
                    label = label_path.read_text().strip() if label_path.exists() else ""
                    if any(x in label for x in ("Package", "Tdie", "Tccd")):
                        return float(inp.read_text().strip()) / 1000.0
            except (OSError, ValueError):
                continue
        return 0.0

    def _read_rapl_power(self) -> tuple[float, float, float, float]:
        """Read RAPL power counters. Returns (package, core, uncore, dram) watts."""
        rapl_base = Path("/sys/class/powercap")
        if not rapl_base.exists():
            return 0.0, 0.0, 0.0, 0.0
        pkg = core = uncore = dram = 0.0
        try:
            for zone in sorted(rapl_base.iterdir()):
                name_path = zone / "name"
                energy_path = zone / "energy_uj"
                if not name_path.exists() or not energy_path.exists():
                    continue
                name = name_path.read_text().strip()
                # Sample energy over 100ms for instantaneous power
                e1 = int(energy_path.read_text().strip())
                time.sleep(0.1)
                e2 = int(energy_path.read_text().strip())
                power = (e2 - e1) / 1e5  # uj/0.1s → watts
                if "package" in name and "core" not in name:
                    pkg += power
                elif "core" in name:
                    core += power
                elif "uncore" in name:
                    uncore += power
                elif "dram" in name:
                    dram += power
        except (OSError, ValueError):
            pass
        return pkg, core, uncore, dram

    def _read_vcore(self) -> float:
        """Read CPU core voltage from hwmon (in0 or labeled VCore)."""
        hwmon_base = Path("/sys/class/hwmon")
        if not hwmon_base.exists():
            return 0.0
        for hwmon in sorted(hwmon_base.iterdir()):
            try:
                name = (hwmon / "name").read_text().strip()
                if name not in ("nct6775", "nct6779", "nct6776", "nct6791",
                                "nct6792", "nct6793", "nct6795", "nct6796",
                                "nct6797", "nct6798", "it8720f", "it8728f",
                                "it8771e", "w83627ehf", "asus_ec"):
                    continue
                for inp in sorted(hwmon.glob("in*_input")):
                    label_path = inp.parent / inp.name.replace("input", "label")
                    label = label_path.read_text().strip() if label_path.exists() else ""
                    if "vcore" in label.lower() or "cpu" in label.lower():
                        return float(inp.read_text().strip()) / 1000.0
            except (OSError, ValueError):
                continue
        return 0.0

    # ── Memory ───────────────────────────────────────────────────────────

    def _collect_memory(self) -> list[MemorySlotInfo]:
        slots: list[MemorySlotInfo] = []
        try:
            dmi = subprocess.run(
                ["dmidecode", "-t", "memory"],
                capture_output=True, text=True, timeout=5,
            )
            if dmi.returncode != 0:
                return slots

            for block in dmi.stdout.split("\n\n"):
                if "Memory Device" not in block:
                    continue
                if "No Module Installed" in block:
                    continue
                slot = MemorySlotInfo()
                for line in block.splitlines():
                    line = line.strip()
                    k, _, v = line.partition(":")
                    k = k.strip()
                    v = v.strip()
                    match k:
                        case "Locator":
                            slot.slot = v
                        case "Bank Locator":
                            slot.bank = v
                        case "Size":
                            m = re.match(r"(\d+(?:\.\d+)?)\s*(MB|GB|TB)", v, re.I)
                            if m:
                                factor = {"MB": 1/1024, "GB": 1, "TB": 1024}[m.group(2).upper()]
                                slot.size_gb = float(m.group(1)) * factor
                        case "Type":
                            slot.type = v
                        case "Speed":
                            m = re.search(r"(\d+)", v)
                            slot.speed_mhz = int(m.group(1)) if m else 0
                        case "Configured Memory Speed":
                            m = re.search(r"(\d+)", v)
                            slot.configured_speed_mhz = int(m.group(1)) if m else 0
                        case "Manufacturer":
                            slot.manufacturer = v
                        case "Part Number":
                            slot.part_number = v.strip()
                        case "Serial Number":
                            slot.serial_number = v
                        case "Form Factor":
                            slot.form_factor = v
                        case "Rank":
                            slot.rank = v
                        case "Data Width" | "Total Width":
                            pass
                        case "Configured Voltage":
                            m = re.search(r"([\d.]+)", v)
                            if m:
                                slot.timing.voltage_v = float(m.group(1))
                        case "CAS Latency":
                            m = re.search(r"(\d+)", v)
                            if m:
                                slot.timing.cl = int(m.group(1))
                if slot.size_gb > 0:
                    slots.append(slot)

            # Memory temperature from hwmon jc42 (JEDEC JC42.4 thermal sensor on DIMMs)
            hwmon_base = Path("/sys/class/hwmon")
            if hwmon_base.exists():
                dimm_temps: list[float] = []
                for hwmon in sorted(hwmon_base.iterdir()):
                    try:
                        if (hwmon / "name").read_text().strip() == "jc42":
                            t = float((hwmon / "temp1_input").read_text().strip()) / 1000.0
                            dimm_temps.append(t)
                    except (OSError, ValueError):
                        pass
                for i, slot in enumerate(slots):
                    if i < len(dimm_temps):
                        slot.temperature_c = dimm_temps[i]
        except (OSError, FileNotFoundError):
            pass
        return slots

    def _total_ram_gb(self) -> float:
        try:
            for line in Path("/proc/meminfo").read_text().splitlines():
                if line.startswith("MemTotal:"):
                    kb = int(line.split()[1])
                    return kb / (1024 * 1024)
        except OSError:
            pass
        return 0.0

    # ── GPU ──────────────────────────────────────────────────────────────

    def _collect_gpus(self) -> list[GpuInfo]:
        gpus: list[GpuInfo] = []

        # NVIDIA via nvidia-smi (extended fields)
        try:
            nvsmi = subprocess.run(
                [
                    "nvidia-smi",
                    "--query-gpu=index,name,memory.total,memory.used,driver_version,"
                    "temperature.gpu,temperature.memory,clocks.current.graphics,"
                    "clocks.current.memory,clocks.current.sm,power.draw,"
                    "power.limit,fan.speed,utilization.gpu,utilization.nvenc,"
                    "utilization.nvdec,pcie.link.width.current,pcie.link.gen.current,"
                    "vbios_version",
                    "--format=csv,noheader,nounits",
                ],
                capture_output=True, text=True, timeout=5,
            )
            if nvsmi.returncode == 0:
                for line in nvsmi.stdout.strip().splitlines():
                    parts = [p.strip() for p in line.split(",")]
                    if len(parts) < 19:
                        continue
                    def _f(x: str) -> float:
                        try:
                            return float(x)
                        except (ValueError, TypeError):
                            return 0.0
                    gpu = GpuInfo(
                        index=int(_f(parts[0])),
                        vendor="NVIDIA",
                        model=parts[1],
                        vram_gb=_f(parts[2]) / 1024,
                        vram_used_gb=_f(parts[3]) / 1024,
                        driver_version=parts[4],
                        temperature_c=_f(parts[5]),
                        vram_temp_c=_f(parts[6]),
                        core_clock_mhz=_f(parts[7]),
                        memory_clock_mhz=_f(parts[8]),
                        shader_clock_mhz=_f(parts[9]),
                        power_w=_f(parts[10]),
                        power_limit_w=_f(parts[11]),
                        fan_percent=_f(parts[12]),
                        utilization_percent=_f(parts[13]),
                        nvenc_usage_percent=_f(parts[14]),
                        nvdec_usage_percent=_f(parts[15]),
                        pcie_width=int(_f(parts[16])),
                        pcie_gen=int(_f(parts[17])),
                        bios_version=parts[18] if len(parts) > 18 else "",
                    )
                    gpus.append(gpu)
        except (OSError, FileNotFoundError):
            pass

        # AMD via hwmon (amdgpu) if no NVIDIA found or for discrete AMD
        if not gpus:
            gpus.extend(self._collect_amd_gpus())

        return gpus

    def _collect_amd_gpus(self) -> list[GpuInfo]:
        gpus: list[GpuInfo] = []
        hwmon_base = Path("/sys/class/hwmon")
        if not hwmon_base.exists():
            return gpus
        idx = 0
        for hwmon in sorted(hwmon_base.iterdir()):
            try:
                name = (hwmon / "name").read_text().strip()
                if name != "amdgpu":
                    continue
                gpu = GpuInfo(index=idx, vendor="AMD")
                # Model from device/product_name or PCI subsystem
                device_path = hwmon / "device"
                for fname in ("product_name", "label"):
                    p = device_path / fname
                    if p.exists():
                        gpu.model = p.read_text().strip()
                        break
                # Temperature
                for inp in sorted(hwmon.glob("temp*_input")):
                    label_p = inp.parent / inp.name.replace("input", "label")
                    label = label_p.read_text().strip() if label_p.exists() else ""
                    val = float(inp.read_text().strip()) / 1000.0
                    if "edge" in label.lower() or "junction" not in label.lower():
                        gpu.temperature_c = val
                    elif "junction" in label.lower() or "hotspot" in label.lower():
                        gpu.hotspot_temp_c = val
                    elif "mem" in label.lower():
                        gpu.vram_temp_c = val
                # Power
                for power in sorted(hwmon.glob("power*_average")):
                    try:
                        gpu.power_w = float(power.read_text().strip()) / 1e6
                        break
                    except (ValueError, OSError):
                        pass
                # Fan
                fan_path = hwmon / "fan1_input"
                if fan_path.exists():
                    gpu.fan_rpm = int(fan_path.read_text().strip())
                # GPU clock via pp_dpm_sclk
                sclk = device_path / "pp_dpm_sclk"
                if sclk.exists():
                    for line in sclk.read_text().splitlines():
                        if "*" in line:
                            m = re.search(r"(\d+)Mhz", line)
                            if m:
                                gpu.core_clock_mhz = float(m.group(1))
                # VRAM
                vram_total = device_path / "mem_info_vram_total"
                vram_used = device_path / "mem_info_vram_used"
                if vram_total.exists():
                    gpu.vram_gb = int(vram_total.read_text().strip()) / 1e9
                if vram_used.exists():
                    gpu.vram_used_gb = int(vram_used.read_text().strip()) / 1e9
                gpus.append(gpu)
                idx += 1
            except (OSError, ValueError):
                continue
        return gpus

    # ── Storage ──────────────────────────────────────────────────────────

    def _collect_storage(self) -> list[StorageInfo]:
        devices: list[StorageInfo] = []
        block = Path("/sys/block")
        if not block.exists():
            return devices

        now = time.time()
        for dev_path in sorted(block.iterdir()):
            name = dev_path.name
            if re.match(r"(loop|ram|dm-|sr|fd|zram)", name):
                continue
            try:
                size_sectors = int((dev_path / "size").read_text().strip())
                if size_sectors == 0:
                    continue
            except (OSError, ValueError):
                continue

            dev = StorageInfo(device=f"/dev/{name}")
            dev.capacity_bytes = size_sectors * 512

            # Model, serial, rotational, interface
            dpath = dev_path / "device"
            for fname, attr in [("model", "model"), ("serial", "serial"),
                                  ("vendor", ""), ("firmware_rev", "firmware")]:
                p = dpath / fname
                if p.exists():
                    val = p.read_text().strip()
                    if attr == "model":
                        dev.model = val
                    elif attr == "serial":
                        dev.serial = val
                    elif attr == "firmware":
                        dev.firmware = val

            try:
                dev.rotational = bool(int((dev_path / "queue" / "rotational").read_text().strip()))
            except (OSError, ValueError):
                pass

            # Interface: NVMe vs SATA vs USB
            if name.startswith("nvme"):
                dev.interface = "NVMe"
            elif (dpath / "transport").exists():
                dev.interface = (dpath / "transport").read_text().strip().upper()
            else:
                dev.interface = "SATA" if not dev.rotational else "HDD/SATA"

            # Read/write rates from /sys/block/*/stat
            try:
                stat = (dev_path / "stat").read_text().split()
                # fields: reads_completed, reads_merged, sectors_read, ms_read,
                #         writes_completed, writes_merged, sectors_written, ms_written, ...
                r_sectors = int(stat[2])
                w_sectors = int(stat[6])
                prev = self._prev_disk.get(name)
                if prev:
                    pr, pw, pt = prev
                    dt = now - pt
                    if dt > 0:
                        dev.read_rate_mbs = (r_sectors - pr) * 512 / 1e6 / dt
                        dev.write_rate_mbs = (w_sectors - pw) * 512 / 1e6 / dt
                self._prev_disk[name] = (r_sectors, w_sectors, now)
            except (OSError, ValueError, IndexError):
                pass

            # SMART data
            dev.smart = self._collect_smart(name, dev.interface)
            if dev.smart:
                dev.temperature_c = dev.smart.temperature_c

            devices.append(dev)
        return devices

    def _collect_smart(self, device: str, interface: str) -> SmartData | None:
        try:
            result = subprocess.run(
                ["smartctl", "-x", "--json=c", f"/dev/{device}"],
                capture_output=True, text=True, timeout=10,
            )
            if result.returncode & ~0x01:  # bit 0 = SMART pre-fail warning, that's OK
                return None
            data = json.loads(result.stdout)
        except (OSError, FileNotFoundError, json.JSONDecodeError):
            return None

        smart = SmartData()

        # Overall health
        test_result = data.get("smart_status", {})
        smart.overall_status = "PASSED" if test_result.get("passed", True) else "FAILED"

        # Power-on hours
        poh = data.get("power_on_time", {})
        smart.power_on_hours = poh.get("hours", 0)

        # Temperature
        temp = data.get("temperature", {})
        smart.temperature_c = float(temp.get("current", 0))

        # ATA attributes
        ata_attrs = data.get("ata_smart_attributes", {}).get("table", [])
        for attr in ata_attrs:
            sa = SmartAttribute(
                id=attr.get("id", 0),
                name=attr.get("name", ""),
                value=attr.get("value", 0),
                worst=attr.get("worst", 0),
                raw=attr.get("raw", {}).get("value", 0),
                threshold=attr.get("thresh", 0),
                flags=attr.get("flags", {}).get("value", 0),
                pre_fail=attr.get("flags", {}).get("prefailure", False),
            )
            sa.failing = sa.value != 0 and sa.value <= sa.threshold and sa.pre_fail
            smart.attributes.append(sa)
            # Extract key attrs
            if attr["id"] == 5:
                smart.reallocated_sectors = sa.raw
            elif attr["id"] == 197:
                smart.pending_sectors = sa.raw
            elif attr["id"] == 198:
                smart.uncorrectable_errors = sa.raw

        # Calculate health from reallocated/pending/uncorrectable
        penalty = (smart.reallocated_sectors * 2 +
                   smart.pending_sectors * 2 +
                   smart.uncorrectable_errors * 5)
        smart.health_percent = max(0, 100 - min(penalty, 100))

        # NVMe data
        nvme_health = data.get("nvme_smart_health_information_log", {})
        if nvme_health:
            smart.nvme = NvmeData(
                temperature_c=smart.temperature_c,
                available_spare_percent=nvme_health.get("available_spare", 0),
                available_spare_threshold_percent=nvme_health.get("available_spare_threshold", 0),
                percentage_used=nvme_health.get("percentage_used", 0),
                data_units_written=nvme_health.get("data_units_written", 0),
                data_units_read=nvme_health.get("data_units_read", 0),
                host_write_commands=nvme_health.get("host_write_commands", 0),
                host_read_commands=nvme_health.get("host_read_commands", 0),
                media_errors=nvme_health.get("media_errors", 0),
                num_err_log_entries=nvme_health.get("num_err_log_entries", 0),
                power_on_hours=smart.power_on_hours,
                unsafe_shutdowns=nvme_health.get("unsafe_shutdowns", 0),
                critical_warning=nvme_health.get("critical_warning", 0),
            )
            smart.health_percent = max(0, 100 - smart.nvme.percentage_used)
            if smart.nvme.critical_warning:
                smart.overall_status = "FAILED"

        return smart

    # ── Motherboard ───────────────────────────────────────────────────────

    def _collect_motherboard(self) -> MotherboardInfo:
        mb = MotherboardInfo()
        dmi_fields = {
            "/sys/class/dmi/id/board_vendor": "manufacturer",
            "/sys/class/dmi/id/board_name": "model",
            "/sys/class/dmi/id/board_version": "version",
            "/sys/class/dmi/id/bios_vendor": "bios_vendor",
            "/sys/class/dmi/id/bios_version": "bios_version",
            "/sys/class/dmi/id/bios_date": "bios_date",
        }
        for path, attr in dmi_fields.items():
            try:
                setattr(mb, attr, Path(path).read_text().strip())
            except OSError:
                pass

        # Chipset from lspci
        try:
            lspci = subprocess.run(
                ["lspci", "-mm", "-d", "::0600"],  # host bridge = chipset
                capture_output=True, text=True, timeout=3,
            )
            if lspci.returncode == 0 and lspci.stdout:
                first = lspci.stdout.splitlines()[0]
                # Format: "00:00.0 "Host bridge" "Intel Corporation" "..."
                parts = re.findall(r'"([^"]*)"', first)
                if len(parts) >= 3:
                    mb.chipset = f"{parts[1]} {parts[2]}".strip()
        except (OSError, FileNotFoundError):
            pass

        # VRM / PCH temperatures from hwmon
        hwmon_base = Path("/sys/class/hwmon")
        if hwmon_base.exists():
            for hwmon in sorted(hwmon_base.iterdir()):
                try:
                    name = (hwmon / "name").read_text().strip()
                    if name not in ("nct6775", "nct6779", "nct6776", "nct6791",
                                    "nct6792", "nct6793", "nct6795", "nct6796",
                                    "nct6797", "nct6798", "asus_ec", "it8720f",
                                    "it8728f", "it8771e", "w83627ehf"):
                        continue
                    for inp in sorted(hwmon.glob("temp*_input")):
                        label_path = inp.parent / inp.name.replace("input", "label")
                        label = label_path.read_text().strip() if label_path.exists() else ""
                        try:
                            val = float(inp.read_text().strip()) / 1000.0
                        except (ValueError, OSError):
                            continue
                        ll = label.lower()
                        if "vrm" in ll or "vcore" in ll:
                            mb.vrm_temp_c = val
                        elif "pch" in ll:
                            mb.pch_temp_c = val
                        elif "ambient" in ll or "system" in ll or "board" in ll:
                            if not mb.ambient_temp_c:
                                mb.ambient_temp_c = val
                except OSError:
                    continue
        return mb

    # ── Fans + Voltages ───────────────────────────────────────────────────

    def _collect_hwmon_fans_voltages(self) -> tuple[list[FanReading], list[VoltageReading]]:
        fans: list[FanReading] = []
        voltages: list[VoltageReading] = []
        hwmon_base = Path("/sys/class/hwmon")
        if not hwmon_base.exists():
            return fans, voltages

        for hwmon in sorted(hwmon_base.iterdir()):
            try:
                chip_name = (hwmon / "name").read_text().strip()
            except OSError:
                continue

            # Fan inputs
            for inp in sorted(hwmon.glob("fan*_input")):
                idx = re.search(r"fan(\d+)_input", inp.name)
                if not idx:
                    continue
                n = idx.group(1)
                label_path = hwmon / f"fan{n}_label"
                label = label_path.read_text().strip() if label_path.exists() else f"{chip_name} Fan{n}"
                try:
                    rpm = int(inp.read_text().strip())
                except (ValueError, OSError):
                    continue
                if rpm < 0 or rpm > 20000:
                    continue
                # Target RPM for controllable fans
                target_path = hwmon / f"fan{n}_target"
                target = 0
                if target_path.exists():
                    try:
                        target = int(target_path.read_text().strip())
                    except (ValueError, OSError):
                        pass
                fans.append(FanReading(
                    name=label, rpm=rpm, target_rpm=target,
                    controllable=target_path.exists(),
                ))

            # Voltage inputs
            for inp in sorted(hwmon.glob("in*_input")):
                idx = re.search(r"in(\d+)_input", inp.name)
                if not idx:
                    continue
                n = idx.group(1)
                label_path = hwmon / f"in{n}_label"
                label = label_path.read_text().strip() if label_path.exists() else f"{chip_name} V{n}"
                try:
                    mv = float(inp.read_text().strip())
                except (ValueError, OSError):
                    continue
                v = mv / 1000.0
                if v <= 0 or v > 25:
                    continue
                min_v = max_v = 0.0
                try:
                    min_v = float((hwmon / f"in{n}_min").read_text().strip()) / 1000.0
                    max_v = float((hwmon / f"in{n}_max").read_text().strip()) / 1000.0
                except (OSError, ValueError):
                    pass
                voltages.append(VoltageReading(
                    name=label, voltage_v=v, min_v=min_v, max_v=max_v
                ))

        return fans, voltages

    # ── Battery ───────────────────────────────────────────────────────────

    def _collect_batteries(self) -> list[BatteryInfo]:
        bats: list[BatteryInfo] = []
        ps_base = Path("/sys/class/power_supply")
        if not ps_base.exists():
            return bats
        for ps in sorted(ps_base.iterdir()):
            try:
                ps_type = (ps / "type").read_text().strip()
                if ps_type != "Battery":
                    continue
                bat = BatteryInfo(name=ps.name)
                def _ri(fname: str) -> int:
                    p = ps / fname
                    return int(p.read_text().strip()) if p.exists() else 0
                def _rs(fname: str) -> str:
                    p = ps / fname
                    return p.read_text().strip() if p.exists() else ""
                bat.status = _rs("status")
                bat.technology = _rs("technology")
                bat.cycle_count = _ri("cycle_count")
                bat.voltage_mv = _ri("voltage_now") // 1000
                # Energy units (preferred) or charge units
                if (ps / "energy_full_design").exists():
                    bat.capacity_design_mwh = _ri("energy_full_design") // 1000
                    bat.capacity_full_mwh = _ri("energy_full") // 1000
                    bat.capacity_now_mwh = _ri("energy_now") // 1000
                    bat.charge_rate_mw = _ri("power_now") // 1000
                elif (ps / "charge_full_design").exists():
                    # Convert charge (µAh) to energy (mWh) using voltage
                    v_uv = _ri("voltage_now") or 3_700_000
                    bat.capacity_design_mwh = _ri("charge_full_design") * v_uv // 1_000_000_000
                    bat.capacity_full_mwh = _ri("charge_full") * v_uv // 1_000_000_000
                    bat.capacity_now_mwh = _ri("charge_now") * v_uv // 1_000_000_000
                    bat.charge_rate_mw = _ri("current_now") * v_uv // 1_000_000_000
                bats.append(bat)
            except (OSError, ValueError):
                continue
        return bats

    # ── PCIe topology ─────────────────────────────────────────────────────

    def _collect_pcie(self) -> list[PcieDevice]:
        devices: list[PcieDevice] = []
        try:
            out = subprocess.run(
                ["lspci", "-vmm"], capture_output=True, text=True, timeout=5,
            )
            if out.returncode != 0:
                return devices
            current: dict[str, str] = {}
            for line in out.stdout.splitlines():
                if not line.strip():
                    if current:
                        d = PcieDevice(
                            slot=current.get("Slot", ""),
                            class_name=current.get("Class", ""),
                            vendor=current.get("Vendor", ""),
                            device=current.get("Device", ""),
                            subsystem=current.get("SDevice", ""),
                            driver=current.get("Driver", ""),
                        )
                        # Check PCIe link width for this device
                        slot_id = d.slot.replace(":", "/").replace(".", "/")
                        link_path = Path(f"/sys/bus/pci/devices/0000:{d.slot}")
                        if link_path.exists():
                            try:
                                d.width = int((link_path / "current_link_width").read_text().strip())
                                d.gen = int((link_path / "current_link_speed").read_text().strip().split(".")[0])
                            except (OSError, ValueError):
                                pass
                        devices.append(d)
                        current = {}
                elif ":" in line:
                    k, _, v = line.partition(":")
                    current[k.strip()] = v.strip()
        except (OSError, FileNotFoundError):
            pass
        return devices

    # ── Network ───────────────────────────────────────────────────────────

    def _collect_network(self) -> list[NetworkInfo]:
        nics: list[NetworkInfo] = []
        net_base = Path("/sys/class/net")
        if not net_base.exists():
            return nics
        now = time.time()
        for iface in sorted(net_base.iterdir()):
            name = iface.name
            if name == "lo":
                continue
            nic = NetworkInfo(name=name)
            # MAC
            try:
                nic.mac = (iface / "address").read_text().strip()
            except OSError:
                pass
            # Speed
            try:
                nic.speed_mbps = int((iface / "speed").read_text().strip())
            except (OSError, ValueError):
                pass
            # Link state
            try:
                nic.link_up = (iface / "operstate").read_text().strip() == "up"
            except OSError:
                pass
            # Driver
            driver_link = iface / "device" / "driver"
            if driver_link.is_symlink():
                nic.driver = Path(os.readlink(driver_link)).name
            # RX/TX bytes and rates
            try:
                rx = int((iface / "statistics" / "rx_bytes").read_text().strip())
                tx = int((iface / "statistics" / "tx_bytes").read_text().strip())
                rx_err = int((iface / "statistics" / "rx_errors").read_text().strip())
                tx_err = int((iface / "statistics" / "tx_errors").read_text().strip())
                nic.rx_bytes = rx
                nic.tx_bytes = tx
                nic.rx_errors = rx_err
                nic.tx_errors = tx_err
                prev = self._prev_net.get(name)
                if prev:
                    prx, ptx, pt = prev
                    dt = now - pt
                    if dt > 0:
                        nic.rx_rate_mbs = (rx - prx) / 1e6 / dt
                        nic.tx_rate_mbs = (tx - ptx) / 1e6 / dt
                self._prev_net[name] = (rx, tx, now)
            except (OSError, ValueError):
                pass
            nics.append(nic)
        return nics


# ── Windows backend ────────────────────────────────────────────────────────────

class WindowsHardwareCollector:
    """
    Collect hardware info and sensors on Windows via WMI + LibreHardwareMonitor.

    Deep sensor access (per-core temps, voltages, fan curves, power) requires
    LibreHardwareMonitor (LHM) which exposes a WMI provider at root\\LibreHardwareMonitor.
    If LHM is not running, the agent starts a bundled copy automatically.

    Basic inventory (CPU model, RAM, disks, GPU model) works without LHM via
    standard WMI Win32_* classes.
    """

    LHM_WMI_NS = "root\\LibreHardwareMonitor"
    _lhm_started = False

    def collect(self) -> HardwareSnapshot:
        snap = HardwareSnapshot(
            timestamp=time.time(),
            hostname=platform.node(),
            os=platform.system(),
            os_version=platform.version(),
        )
        # Ensure LHM is running for deep sensor access
        if not self._lhm_started:
            self._ensure_lhm_running()

        snap.cpu = self._collect_cpu()
        snap.memory_slots = self._collect_memory()
        snap.total_memory_gb = sum(s.size_gb for s in snap.memory_slots)
        snap.gpus = self._collect_gpus()
        snap.storage = self._collect_storage()
        snap.motherboard = self._collect_motherboard()
        snap.batteries = self._collect_batteries()
        snap.network = self._collect_network()

        # Deep sensors from LHM WMI
        lhm_sensors = self._collect_lhm_sensors()
        self._apply_lhm_sensors(snap, lhm_sensors)

        return snap

    def _wmi_query(self, query: str, namespace: str = "root\\cimv2") -> list[dict]:
        """Run a WMI query via PowerShell and return list of dicts."""
        script = f"""
$result = Get-WmiObject -Namespace '{namespace}' -Query "{query}" -ErrorAction SilentlyContinue
if ($result) {{ $result | Select-Object * | ConvertTo-Json -Depth 3 -Compress }}
"""
        try:
            proc = subprocess.run(
                ["powershell.exe", "-NonInteractive", "-NoProfile", "-Command", script],
                capture_output=True, text=True, timeout=30,
            )
            if proc.returncode != 0 or not proc.stdout.strip():
                return []
            data = json.loads(proc.stdout.strip())
            if isinstance(data, dict):
                data = [data]
            return data if isinstance(data, list) else []
        except (subprocess.TimeoutExpired, json.JSONDecodeError, Exception):
            return []

    def _ensure_lhm_running(self) -> None:
        """Start LibreHardwareMonitor WMI server if not already running."""
        try:
            rows = self._wmi_query("SELECT * FROM Hardware", self.LHM_WMI_NS)
            if rows:
                self._lhm_started = True
                return
        except Exception:
            pass

        # Try to start bundled LHM server
        lhm_exe = Path(__file__).parent / "bin" / "LibreHardwareMonitorServer.exe"
        if lhm_exe.exists():
            try:
                subprocess.Popen(
                    [str(lhm_exe), "--background"],
                    creationflags=0x00000008,  # DETACHED_PROCESS
                )
                time.sleep(2)  # Wait for WMI provider to register
                self._lhm_started = True
                log.info("LibreHardwareMonitor WMI server started")
            except Exception as e:
                log.debug("Could not start LHM server: %s", e)

    def _collect_cpu(self) -> CpuPackageInfo:
        cpu = CpuPackageInfo()
        rows = self._wmi_query(
            "SELECT Name, NumberOfCores, NumberOfLogicalProcessors, "
            "MaxClockSpeed, CurrentClockSpeed, SocketDesignation "
            "FROM Win32_Processor"
        )
        if rows:
            row = rows[0]
            cpu.brand = row.get("Name", "").strip()
            cpu.model = cpu.brand
            cpu.physical_cores = int(row.get("NumberOfCores", 0))
            cpu.logical_cores = int(row.get("NumberOfLogicalProcessors", 0))
            cpu.base_clock_mhz = float(row.get("MaxClockSpeed", 0))
            cpu.socket = row.get("SocketDesignation", "")
            cpu.sockets = len(rows)
        return cpu

    def _collect_memory(self) -> list[MemorySlotInfo]:
        slots: list[MemorySlotInfo] = []
        rows = self._wmi_query(
            "SELECT DeviceLocator, BankLabel, Capacity, Speed, ConfiguredClockSpeed, "
            "MemoryType, SMBIOSMemoryType, Manufacturer, PartNumber, SerialNumber, "
            "FormFactor, TypeDetail, ConfiguredVoltage "
            "FROM Win32_PhysicalMemory"
        )
        type_map = {
            20: "DDR", 21: "DDR2", 22: "DDR2 FB-DIMM", 24: "DDR3",
            26: "DDR4", 34: "DDR5", 0: "Unknown",
        }
        for row in rows:
            cap = row.get("Capacity")
            if not cap:
                continue
            slot = MemorySlotInfo(
                slot=row.get("DeviceLocator", ""),
                bank=row.get("BankLabel", ""),
                size_gb=int(cap) / 1e9,
                speed_mhz=int(row.get("Speed") or 0),
                configured_speed_mhz=int(row.get("ConfiguredClockSpeed") or 0),
                manufacturer=str(row.get("Manufacturer", "")).strip(),
                part_number=str(row.get("PartNumber", "")).strip(),
                serial_number=str(row.get("SerialNumber", "")).strip(),
            )
            mt = int(row.get("SMBIOSMemoryType") or row.get("MemoryType") or 0)
            slot.type = type_map.get(mt, f"Type{mt}")
            ff = {8: "DIMM", 12: "SO-DIMM", 13: "Micro-DIMM"}.get(
                int(row.get("FormFactor") or 0), "")
            slot.form_factor = ff
            volt = row.get("ConfiguredVoltage")
            if volt:
                slot.timing.voltage_v = int(volt) / 1000.0
            slots.append(slot)
        return slots

    def _collect_gpus(self) -> list[GpuInfo]:
        # Try nvidia-smi first (more detail)
        linux = LinuxHardwareCollector()
        gpus = linux._collect_gpus()
        if gpus:
            return gpus

        # Fall back to Win32_VideoController
        rows = self._wmi_query(
            "SELECT Name, AdapterRAM, DriverVersion, VideoModeDescription "
            "FROM Win32_VideoController"
        )
        for i, row in enumerate(rows):
            name = row.get("Name", "")
            ram = row.get("AdapterRAM")
            gpu = GpuInfo(
                index=i,
                vendor="NVIDIA" if "NVIDIA" in name.upper() else
                       "AMD" if "AMD" in name.upper() or "Radeon" in name else
                       "Intel" if "Intel" in name else "",
                model=name,
                vram_gb=int(ram) / 1e9 if ram else 0.0,
                driver_version=row.get("DriverVersion", ""),
            )
            gpus.append(gpu)
        return gpus

    def _collect_storage(self) -> list[StorageInfo]:
        devices: list[StorageInfo] = []
        rows = self._wmi_query(
            "SELECT DeviceID, Model, SerialNumber, FirmwareRevision, "
            "Size, MediaType, InterfaceType "
            "FROM Win32_DiskDrive"
        )
        for row in rows:
            size = row.get("Size")
            dev = StorageInfo(
                device=row.get("DeviceID", ""),
                model=str(row.get("Model", "")).strip(),
                serial=str(row.get("SerialNumber", "")).strip(),
                firmware=str(row.get("FirmwareRevision", "")).strip(),
                interface=str(row.get("InterfaceType", "")).strip(),
                capacity_bytes=int(size) if size else 0,
                rotational=str(row.get("MediaType", "")).lower() != "ssd",
            )
            # Try smartctl for SMART data
            dev_id = dev.device.replace("\\\\.\\", "")
            dev.smart = self._collect_smart_windows(dev_id)
            if dev.smart:
                dev.temperature_c = dev.smart.temperature_c
            devices.append(dev)
        return devices

    def _collect_smart_windows(self, device_id: str) -> SmartData | None:
        try:
            result = subprocess.run(
                ["smartctl", "-x", "--json=c", f"\\\\.\\{device_id}"],
                capture_output=True, text=True, timeout=10,
            )
            if result.returncode & ~0x01:
                return None
            data = json.loads(result.stdout)
            # Reuse Linux parser logic
            lc = LinuxHardwareCollector()
            return lc._collect_smart.__func__(lc, device_id, "")  # type: ignore
        except Exception:
            return None

    def _collect_motherboard(self) -> MotherboardInfo:
        mb = MotherboardInfo()
        rows = self._wmi_query(
            "SELECT Manufacturer, Product, Version FROM Win32_BaseBoard"
        )
        if rows:
            mb.manufacturer = str(rows[0].get("Manufacturer", "")).strip()
            mb.model = str(rows[0].get("Product", "")).strip()
            mb.version = str(rows[0].get("Version", "")).strip()
        bios_rows = self._wmi_query(
            "SELECT SMBIOSBIOSVersion, Manufacturer, ReleaseDate FROM Win32_BIOS"
        )
        if bios_rows:
            mb.bios_version = str(bios_rows[0].get("SMBIOSBIOSVersion", "")).strip()
            mb.bios_vendor = str(bios_rows[0].get("Manufacturer", "")).strip()
            mb.bios_date = str(bios_rows[0].get("ReleaseDate", "")).strip()
        return mb

    def _collect_batteries(self) -> list[BatteryInfo]:
        bats: list[BatteryInfo] = []
        rows = self._wmi_query(
            "SELECT Name, BatteryStatus, DesignCapacity, FullChargeCapacity, "
            "RemainingCapacity, TimeToFullCharge, TimeOnBattery, Chemistry "
            "FROM Win32_Battery"
        )
        for row in rows:
            status_map = {1: "Discharging", 2: "AC+Battery", 3: "Full",
                          4: "Low", 5: "Critical", 6: "Charging",
                          7: "Charging+High", 8: "Charging+Low", 9: "Charging+Critical"}
            bat = BatteryInfo(
                name=str(row.get("Name", "Battery")).strip(),
                status=status_map.get(int(row.get("BatteryStatus") or 0), "Unknown"),
                capacity_design_mwh=int(row.get("DesignCapacity") or 0),
                capacity_full_mwh=int(row.get("FullChargeCapacity") or 0),
                capacity_now_mwh=int(row.get("RemainingCapacity") or 0),
            )
            chem_map = {1: "Other", 2: "Unknown", 3: "Lead Acid", 4: "NiCd",
                        5: "NiMH", 6: "Li-ion", 7: "Zinc air", 8: "LiPo"}
            bat.technology = chem_map.get(int(row.get("Chemistry") or 0), "")
            bats.append(bat)
        return bats

    def _collect_network(self) -> list[NetworkInfo]:
        nics: list[NetworkInfo] = []
        rows = self._wmi_query(
            "SELECT Name, MACAddress, Speed, AdapterType FROM Win32_NetworkAdapter "
            "WHERE PhysicalAdapter=True"
        )
        for row in rows:
            speed = row.get("Speed")
            nic = NetworkInfo(
                name=str(row.get("Name", "")).strip(),
                mac=str(row.get("MACAddress", "")).strip(),
                speed_mbps=int(int(speed) / 1e6) if speed else 0,
            )
            nics.append(nic)
        return nics

    def _collect_lhm_sensors(self) -> list[dict]:
        """Collect all sensors from LibreHardwareMonitor WMI namespace."""
        return self._wmi_query(
            "SELECT Name, SensorType, Value, Min, Max, Parent FROM Sensor",
            namespace=self.LHM_WMI_NS,
        )

    def _apply_lhm_sensors(self, snap: HardwareSnapshot, sensors: list[dict]) -> None:
        """Map LHM sensor readings onto the snapshot data structures."""
        for s in sensors:
            name = str(s.get("Name", "")).lower()
            stype = str(s.get("SensorType", "")).lower()
            val = s.get("Value")
            if val is None:
                continue
            try:
                val = float(val)
            except (TypeError, ValueError):
                continue

            if stype == "temperature":
                if "core #" in name:
                    m = re.search(r"core #(\d+)", name)
                    if m:
                        idx = int(m.group(1)) - 1
                        if idx < len(snap.cpu.cores):
                            snap.cpu.cores[idx].temperature_c = val
                elif "package" in name or "cpu" in name and "die" in name:
                    snap.cpu.package_temp_c = max(snap.cpu.package_temp_c, val)
                elif "gpu" in name and "hot spot" in name:
                    for gpu in snap.gpus:
                        gpu.hotspot_temp_c = val
                elif "gpu" in name and "memory" in name:
                    for gpu in snap.gpus:
                        gpu.vram_temp_c = val
                elif "gpu" in name:
                    for gpu in snap.gpus:
                        if not gpu.hotspot_temp_c:
                            gpu.temperature_c = val
                elif "vrm" in name:
                    snap.motherboard.vrm_temp_c = val
                elif "pch" in name:
                    snap.motherboard.pch_temp_c = val

            elif stype == "clock":
                if "core #" in name:
                    m = re.search(r"core #(\d+)", name)
                    if m:
                        idx = int(m.group(1)) - 1
                        if idx < len(snap.cpu.cores):
                            snap.cpu.cores[idx].clock_mhz = val
                elif "gpu core" in name:
                    for gpu in snap.gpus:
                        gpu.core_clock_mhz = val
                elif "gpu memory" in name:
                    for gpu in snap.gpus:
                        gpu.memory_clock_mhz = val

            elif stype == "voltage":
                if "vcore" in name or "cpu core" in name:
                    snap.cpu.core_voltage_v = val
                else:
                    snap.voltages.append(VoltageReading(
                        name=s.get("Name", name), voltage_v=val,
                        min_v=float(s.get("Min") or 0),
                        max_v=float(s.get("Max") or 0),
                    ))

            elif stype == "fan":
                snap.fans.append(FanReading(
                    name=s.get("Name", name), rpm=int(val)
                ))

            elif stype == "power":
                if "cpu package" in name:
                    snap.cpu.package_power_w = val
                elif "cpu cores" in name:
                    snap.cpu.core_power_w = val
                elif "cpu dram" in name:
                    snap.cpu.dram_power_w = val
                elif "gpu" in name:
                    for gpu in snap.gpus:
                        gpu.power_w = val

            elif stype == "load":
                if "cpu total" in name:
                    pass  # Could set cpu usage
                elif "gpu core" in name:
                    for gpu in snap.gpus:
                        gpu.utilization_percent = val

            elif stype == "throughput":
                if "gpu memory" in name and "read" in name:
                    for gpu in snap.gpus:
                        gpu.memory_bandwidth_gbs = val / 1024  # MB/s → GB/s

            elif stype == "smalldata":
                if "gpu memory used" in name:
                    for gpu in snap.gpus:
                        gpu.vram_used_gb = val / 1024  # MB → GB


# ── macOS backend ──────────────────────────────────────────────────────────────

class MacOSHardwareCollector:
    """
    Collect hardware info on macOS via sysctl, system_profiler, and IOKit.
    Sensor data (fans, power) requires powermetrics (root) or SMC tools.
    """

    def collect(self) -> HardwareSnapshot:
        snap = HardwareSnapshot(
            timestamp=time.time(),
            hostname=platform.node(),
            os="macOS",
            os_version=platform.mac_ver()[0],
        )
        snap.cpu = self._collect_cpu()
        snap.memory_slots = self._collect_memory()
        snap.total_memory_gb = sum(s.size_gb for s in snap.memory_slots)
        snap.gpus = self._collect_gpus()
        snap.storage = self._collect_storage()
        snap.batteries = self._collect_battery()
        snap.fans = self._collect_fans()
        return snap

    def _sysctl(self, key: str) -> str:
        try:
            out = subprocess.run(
                ["sysctl", "-n", key], capture_output=True, text=True, timeout=3
            )
            return out.stdout.strip() if out.returncode == 0 else ""
        except (OSError, subprocess.TimeoutExpired):
            return ""

    def _collect_cpu(self) -> CpuPackageInfo:
        cpu = CpuPackageInfo()
        cpu.brand = self._sysctl("machdep.cpu.brand_string")
        cpu.model = cpu.brand
        cpu.physical_cores = int(self._sysctl("hw.physicalcpu") or 0)
        cpu.logical_cores = int(self._sysctl("hw.logicalcpu") or 0)
        cpu.base_clock_mhz = int(self._sysctl("hw.cpufrequency_max") or 0) / 1e6 or \
                              int(self._sysctl("hw.tbfrequency") or 0) / 1e6
        # Apple Silicon: no hz sysctl, use nominalFrequencyHz from IOKit
        # flags
        flags_raw = self._sysctl("machdep.cpu.features")
        cpu.instructions = [f.upper() for f in flags_raw.split() if f.upper() in
                            ("AVX", "AVX2", "SSE4.2", "SSE4.1", "AES", "FMA")]
        return cpu

    def _collect_memory(self) -> list[MemorySlotInfo]:
        slots: list[MemorySlotInfo] = []
        try:
            out = subprocess.run(
                ["system_profiler", "SPMemoryDataType", "-json"],
                capture_output=True, text=True, timeout=10,
            )
            data = json.loads(out.stdout)
            for item in data.get("SPMemoryDataType", []):
                for dimm in item.get("_items", []):
                    cap = dimm.get("dimm_size", "0 GB")
                    m = re.match(r"([\d.]+)\s*(MB|GB|TB)", cap, re.I)
                    size = 0.0
                    if m:
                        size = float(m.group(1)) * {"MB": 1/1024, "GB": 1, "TB": 1024}[m.group(2).upper()]
                    slot = MemorySlotInfo(
                        slot=dimm.get("dimm_slot", ""),
                        size_gb=size,
                        type=dimm.get("dimm_type", ""),
                        speed_mhz=int(re.search(r"(\d+)", dimm.get("dimm_speed", "0")).group(1)),
                        manufacturer=dimm.get("dimm_manufacturer", ""),
                        part_number=dimm.get("dimm_part_number", ""),
                        serial_number=dimm.get("dimm_serial_number", ""),
                    )
                    if slot.size_gb > 0:
                        slots.append(slot)
        except (OSError, json.JSONDecodeError):
            total = int(self._sysctl("hw.memsize") or 0)
            if total:
                slots.append(MemorySlotInfo(slot="Main", size_gb=total / 1e9))
        return slots

    def _collect_gpus(self) -> list[GpuInfo]:
        gpus: list[GpuInfo] = []
        try:
            out = subprocess.run(
                ["system_profiler", "SPDisplaysDataType", "-json"],
                capture_output=True, text=True, timeout=10,
            )
            data = json.loads(out.stdout)
            for i, item in enumerate(data.get("SPDisplaysDataType", [])):
                vram = item.get("sppci_model", "")
                gpu = GpuInfo(
                    index=i,
                    model=item.get("sppci_model", ""),
                    vendor="Apple" if "Apple" in item.get("sppci_vendor", "") else
                           "AMD" if "AMD" in item.get("sppci_vendor", "") else
                           "NVIDIA" if "NVIDIA" in item.get("sppci_vendor", "") else "Intel",
                )
                vram_str = item.get("spdisplays_vram", "0 MB")
                m = re.search(r"(\d+)\s*(MB|GB)", vram_str, re.I)
                if m:
                    gpu.vram_gb = float(m.group(1)) * (1 if m.group(2).upper() == "GB" else 1/1024)
                gpus.append(gpu)
        except (OSError, json.JSONDecodeError):
            pass
        return gpus

    def _collect_storage(self) -> list[StorageInfo]:
        devices: list[StorageInfo] = []
        try:
            out = subprocess.run(
                ["system_profiler", "SPStorageDataType", "-json"],
                capture_output=True, text=True, timeout=10,
            )
            data = json.loads(out.stdout)
            for item in data.get("SPStorageDataType", []):
                for disk in item.get("_items", []):
                    cap = disk.get("com.apple.diskmanagement.media.total-size", 0)
                    dev = StorageInfo(
                        device=disk.get("bsd_name", ""),
                        model=disk.get("device_model", "").strip(),
                        serial=disk.get("device_serial", "").strip(),
                        capacity_bytes=int(cap) if cap else 0,
                        interface=disk.get("spnvme_spec_version", "NVMe") if "nvme" in
                                  disk.get("spnvme_spec_version", "").lower() else "SATA",
                    )
                    devices.append(dev)
        except (OSError, json.JSONDecodeError):
            pass
        return devices

    def _collect_battery(self) -> list[BatteryInfo]:
        bats: list[BatteryInfo] = []
        try:
            out = subprocess.run(
                ["system_profiler", "SPPowerDataType", "-json"],
                capture_output=True, text=True, timeout=5,
            )
            data = json.loads(out.stdout)
            for item in data.get("SPPowerDataType", []):
                battery = item.get("sppower_battery_information", [])
                if isinstance(battery, list):
                    battery = battery[0] if battery else {}
                if not battery:
                    continue
                bat = BatteryInfo(name="Battery")
                bat.charge_rate_mw = int(battery.get("sppower_current_capacity", 0))
                cond = battery.get("sppower_battery_health_info", {})
                if isinstance(cond, dict):
                    bat.cycle_count = int(cond.get("sppower_battery_cycle_count", 0))
                    health = cond.get("sppower_battery_health", "")
                    bat.status = "Good" if "Good" in health else health
                bats.append(bat)
        except (OSError, json.JSONDecodeError):
            pass
        return bats

    def _collect_fans(self) -> list[FanReading]:
        fans: list[FanReading] = []
        # Try smckit (pip install smckit) or powermetrics
        try:
            out = subprocess.run(
                ["sudo", "-n", "powermetrics", "--samplers", "smc",
                 "-n", "1", "--format", "plist"],
                capture_output=True, text=True, timeout=5,
            )
            # Parse fan RPM from powermetrics plist output
            for m in re.finditer(r"Fan(\d+)\s+(\d+)\s+RPM", out.stdout):
                fans.append(FanReading(name=f"Fan {m.group(1)}", rpm=int(m.group(2))))
        except (OSError, subprocess.TimeoutExpired):
            pass
        return fans


# ── Cross-platform dispatcher ──────────────────────────────────────────────────

class HardwareInfoCollector:
    """
    Cross-platform hardware info and sensor collector.

    Dispatches to the correct backend based on the current OS. Provides
    a consistent HardwareSnapshot regardless of platform.

    Usage:
        collector = HardwareInfoCollector()
        snap = collector.snapshot()   # full collection (1-5 seconds)
        print(collector.report(snap))
    """

    def __init__(self) -> None:
        system = platform.system()
        if system == "Linux":
            self._backend = LinuxHardwareCollector()
        elif system == "Windows":
            self._backend = WindowsHardwareCollector()
        elif system == "Darwin":
            self._backend = MacOSHardwareCollector()
        else:
            self._backend = LinuxHardwareCollector()

    def snapshot(self) -> HardwareSnapshot:
        """Collect full hardware snapshot. Takes 1-5 seconds (SMART, dmidecode)."""
        return self._backend.collect()

    async def async_snapshot(self) -> HardwareSnapshot:
        """Collect full snapshot in a thread pool (non-blocking)."""
        loop = asyncio.get_event_loop()
        return await loop.run_in_executor(None, self.snapshot)

    # ── Report generation ──────────────────────────────────────────────────

    def report_text(self, snap: HardwareSnapshot) -> str:
        """Generate a HWiNFO64-style text summary report."""
        lines: list[str] = []
        lines.append("=" * 70)
        lines.append(f"  Ozma Hardware Report — {snap.hostname}")
        lines.append(f"  {snap.os} {snap.os_version}")
        lines.append(f"  Generated: {time.strftime('%Y-%m-%d %H:%M:%S', time.localtime(snap.timestamp))}")
        lines.append("=" * 70)

        # CPU
        c = snap.cpu
        lines.append("\n--- CPU " + "-" * 62)
        lines.append(f"  Model:        {c.brand}")
        lines.append(f"  Cores:        {c.physical_cores}C / {c.logical_cores}T  Socket: {c.socket}")
        if c.base_clock_mhz:
            lines.append(f"  Base clock:   {c.base_clock_mhz:.0f} MHz"
                         + (f"  Boost: {c.boost_clock_mhz:.0f} MHz" if c.boost_clock_mhz else ""))
        if c.package_temp_c:
            lines.append(f"  Temperature:  {c.package_temp_c:.1f}°C (package)")
        if c.package_power_w:
            lines.append(f"  Power:        {c.package_power_w:.1f}W package"
                         + (f"  {c.core_power_w:.1f}W cores" if c.core_power_w else ""))
        if c.core_voltage_v:
            lines.append(f"  VCore:        {c.core_voltage_v:.3f}V")
        if c.microcode:
            lines.append(f"  Microcode:    {c.microcode}")
        if c.caches:
            cstr = "  ".join(f"L{ch.level} {ch.size_kb}KB" for ch in c.caches)
            lines.append(f"  Cache:        {cstr}")
        if c.instructions:
            lines.append(f"  Instructions: {' '.join(c.instructions[:12])}")
        if c.cores:
            lines.append(f"  Per-core temps (°C): " +
                         "  ".join(f"#{i}:{core.temperature_c:.0f}"
                                   for i, core in enumerate(c.cores) if core.temperature_c))

        # Memory
        lines.append("\n--- Memory " + "-" * 59)
        lines.append(f"  Total:  {snap.total_memory_gb:.1f} GB")
        for sl in snap.memory_slots:
            lines.append(f"  {sl.slot or 'DIMM'}  {sl.size_gb:.0f}GB  "
                         f"{sl.type}  {sl.configured_speed_mhz or sl.speed_mhz}MT/s"
                         + (f"  {sl.manufacturer} {sl.part_number}".strip() if sl.manufacturer else "")
                         + (f"  {sl.temperature_c:.0f}°C" if sl.temperature_c else ""))

        # GPUs
        if snap.gpus:
            lines.append("\n--- GPU " + "-" * 62)
            for gpu in snap.gpus:
                lines.append(f"  [{gpu.index}] {gpu.vendor} {gpu.model}"
                             + (f"  {gpu.vram_gb:.0f}GB VRAM" if gpu.vram_gb else ""))
                if gpu.temperature_c:
                    s = f"  Temp: {gpu.temperature_c:.1f}°C"
                    if gpu.hotspot_temp_c:
                        s += f"  HotSpot: {gpu.hotspot_temp_c:.1f}°C"
                    if gpu.vram_temp_c:
                        s += f"  VRAM: {gpu.vram_temp_c:.1f}°C"
                    lines.append(s)
                if gpu.core_clock_mhz:
                    lines.append(f"  Clocks: GPU {gpu.core_clock_mhz:.0f}MHz  "
                                 f"Mem {gpu.memory_clock_mhz:.0f}MHz")
                if gpu.power_w:
                    lines.append(f"  Power: {gpu.power_w:.1f}W / {gpu.power_limit_w:.0f}W limit"
                                 + (f"  Fan: {gpu.fan_rpm}RPM ({gpu.fan_percent:.0f}%)"
                                    if gpu.fan_rpm else ""))
                if gpu.vram_used_gb:
                    lines.append(f"  VRAM used: {gpu.vram_used_gb:.1f}GB / {gpu.vram_gb:.1f}GB")
                if gpu.driver_version:
                    lines.append(f"  Driver: {gpu.driver_version}")

        # Storage
        lines.append("\n--- Storage " + "-" * 58)
        for d in snap.storage:
            cap_str = f"{d.capacity_bytes / 1e9:.0f}GB" if d.capacity_bytes else ""
            lines.append(f"  {d.device}  {d.model}  {cap_str}  {d.interface}")
            if d.temperature_c:
                lines.append(f"    Temp: {d.temperature_c:.1f}°C")
            if d.smart:
                s = d.smart
                status_marker = "" if s.overall_status == "PASSED" else f"  *** {s.overall_status} ***"
                lines.append(f"    SMART: {s.health_percent}% health  "
                             f"POH: {s.power_on_hours}h{status_marker}")
                if s.reallocated_sectors or s.pending_sectors or s.uncorrectable_errors:
                    lines.append(f"    !! Reallocated: {s.reallocated_sectors}  "
                                 f"Pending: {s.pending_sectors}  "
                                 f"Uncorrectable: {s.uncorrectable_errors}")
                if s.nvme:
                    n = s.nvme
                    lines.append(f"    NVMe wear: {n.percentage_used}%  "
                                 f"Spare: {n.available_spare_percent}%  "
                                 f"Media errors: {n.media_errors}")

        # Motherboard
        mb = snap.motherboard
        if mb.model:
            lines.append("\n--- Motherboard " + "-" * 54)
            lines.append(f"  {mb.manufacturer} {mb.model} {mb.version}".strip())
            lines.append(f"  BIOS: {mb.bios_vendor} {mb.bios_version}  {mb.bios_date}".strip())
            if mb.chipset:
                lines.append(f"  Chipset: {mb.chipset}")
            temps = []
            if mb.vrm_temp_c:
                temps.append(f"VRM {mb.vrm_temp_c:.1f}°C")
            if mb.pch_temp_c:
                temps.append(f"PCH {mb.pch_temp_c:.1f}°C")
            if mb.ambient_temp_c:
                temps.append(f"Ambient {mb.ambient_temp_c:.1f}°C")
            if temps:
                lines.append(f"  Temps: {' | '.join(temps)}")

        # Fans
        if snap.fans:
            lines.append("\n--- Fans " + "-" * 61)
            for fan in snap.fans:
                lines.append(f"  {fan.name:30s} {fan.rpm:5d} RPM"
                             + (f"  ({fan.percent:.0f}%)" if fan.percent else ""))

        # Voltages
        if snap.voltages:
            lines.append("\n--- Voltages " + "-" * 57)
            for v in snap.voltages:
                lines.append(f"  {v.name:30s} {v.voltage_v:7.3f}V"
                             + (f"  [{v.min_v:.3f}V – {v.max_v:.3f}V]"
                                if v.min_v or v.max_v else ""))

        # Batteries
        if snap.batteries:
            lines.append("\n--- Battery " + "-" * 58)
            for bat in snap.batteries:
                lines.append(f"  {bat.name}  {bat.status}  "
                             f"Health: {bat.health_percent}%  "
                             f"Charge: {bat.charge_percent:.1f}%")
                if bat.cycle_count:
                    lines.append(f"  Cycles: {bat.cycle_count}  "
                                 f"Design: {bat.capacity_design_mwh}mWh  "
                                 f"Full: {bat.capacity_full_mwh}mWh")

        # Network
        if snap.network:
            lines.append("\n--- Network " + "-" * 58)
            for nic in snap.network:
                state = "UP" if nic.link_up else "DOWN"
                lines.append(f"  {nic.name:20s} {state:4s}  {nic.mac}  "
                             + (f"{nic.speed_mbps}Mbps" if nic.speed_mbps else ""))
                if nic.rx_rate_mbs or nic.tx_rate_mbs:
                    lines.append(f"  {'':20s}  RX {nic.rx_rate_mbs:.2f}MB/s  TX {nic.tx_rate_mbs:.2f}MB/s")

        lines.append("\n" + "=" * 70)
        return "\n".join(lines)

    def report_csv(self, snap: HardwareSnapshot) -> str:
        """CSV format: all sensors as rows (compatible with HWiNFO64 CSV export)."""
        output = io.StringIO()
        writer = csv.writer(output)
        writer.writerow(["Time", "Sensor", "Value", "Unit", "Category"])
        ts = time.strftime("%Y-%m-%d %H:%M:%S", time.localtime(snap.timestamp))
        rows = []
        # CPU sensors
        rows.append([ts, "CPU Package Temperature", f"{snap.cpu.package_temp_c:.1f}", "°C", "CPU"])
        rows.append([ts, "CPU Package Power", f"{snap.cpu.package_power_w:.2f}", "W", "CPU"])
        if snap.cpu.core_voltage_v:
            rows.append([ts, "CPU VCore", f"{snap.cpu.core_voltage_v:.3f}", "V", "CPU"])
        for core in snap.cpu.cores:
            if core.temperature_c:
                rows.append([ts, f"CPU Core #{core.index} Temperature", f"{core.temperature_c:.1f}", "°C", "CPU"])
            if core.clock_mhz:
                rows.append([ts, f"CPU Core #{core.index} Clock", f"{core.clock_mhz:.0f}", "MHz", "CPU"])
        # GPU sensors
        for gpu in snap.gpus:
            pfx = f"{gpu.vendor} {gpu.model}"
            rows.extend([
                [ts, f"{pfx} Temperature", f"{gpu.temperature_c:.1f}", "°C", "GPU"],
                [ts, f"{pfx} Core Clock", f"{gpu.core_clock_mhz:.0f}", "MHz", "GPU"],
                [ts, f"{pfx} Memory Clock", f"{gpu.memory_clock_mhz:.0f}", "MHz", "GPU"],
                [ts, f"{pfx} Power", f"{gpu.power_w:.1f}", "W", "GPU"],
                [ts, f"{pfx} Fan Speed", f"{gpu.fan_rpm}", "RPM", "GPU"],
                [ts, f"{pfx} VRAM Used", f"{gpu.vram_used_gb:.2f}", "GB", "GPU"],
            ])
        # Storage
        for d in snap.storage:
            rows.append([ts, f"{d.model or d.device} Temperature", f"{d.temperature_c:.1f}", "°C", "Storage"])
            if d.smart:
                rows.append([ts, f"{d.model or d.device} Health", f"{d.smart.health_percent}", "%", "Storage"])
        # Fans
        for fan in snap.fans:
            rows.append([ts, fan.name, str(fan.rpm), "RPM", "Fan"])
        # Voltages
        for v in snap.voltages:
            rows.append([ts, v.name, f"{v.voltage_v:.3f}", "V", "Voltage"])
        # Battery
        for bat in snap.batteries:
            rows.append([ts, f"{bat.name} Charge", f"{bat.charge_percent:.1f}", "%", "Battery"])
            rows.append([ts, f"{bat.name} Health", f"{bat.health_percent}", "%", "Battery"])
        writer.writerows(rows)
        return output.getvalue()

    def report_html(self, snap: HardwareSnapshot) -> str:
        """HTML report (HWiNFO64-style sensor view with colour coding)."""
        def _row(label: str, value: str, warn: bool = False, error: bool = False) -> str:
            cls = ' style="background:#ffe0e0"' if error else ' style="background:#fff8dc"' if warn else ""
            return f'<tr{cls}><td>{label}</td><td><b>{value}</b></td></tr>'

        sections: list[str] = []
        sections.append(f"""<!DOCTYPE html><html><head>
<meta charset="utf-8"><title>Ozma Hardware Report — {snap.hostname}</title>
<style>
body{{font-family:monospace;font-size:13px;margin:20px;background:#1e1e1e;color:#ddd}}
h2{{color:#7ec8e3;border-bottom:1px solid #444;padding-bottom:4px}}
table{{border-collapse:collapse;width:100%;margin-bottom:16px}}
td{{padding:3px 8px;border:1px solid #333}}
tr:nth-child(even){{background:#252525}}
</style></head><body>
<h1>Ozma Hardware Report</h1>
<p>{snap.hostname} · {snap.os} {snap.os_version} · {time.strftime("%Y-%m-%d %H:%M:%S", time.localtime(snap.timestamp))}</p>""")

        # CPU
        c = snap.cpu
        sections.append("<h2>CPU</h2><table>")
        sections.append(_row("Model", c.brand))
        sections.append(_row("Topology", f"{c.sockets} socket × {c.physical_cores // max(c.sockets,1)}C / {c.logical_cores}T"))
        if c.package_temp_c:
            sections.append(_row("Package Temperature", f"{c.package_temp_c:.1f}°C",
                                  warn=c.package_temp_c > 80, error=c.package_temp_c > 95))
        if c.package_power_w:
            sections.append(_row("Package Power", f"{c.package_power_w:.1f}W"))
        if c.core_voltage_v:
            sections.append(_row("VCore", f"{c.core_voltage_v:.3f}V"))
        for core in c.cores:
            if core.temperature_c:
                sections.append(_row(f"Core #{core.index}", f"{core.temperature_c:.1f}°C · {core.clock_mhz:.0f}MHz",
                                     warn=core.temperature_c > 85, error=core.temperature_c > 95))
        sections.append("</table>")

        # Storage SMART
        sections.append("<h2>Storage</h2><table>")
        for d in snap.storage:
            sections.append(_row(f"{d.device} — {d.model}", f"{d.capacity_bytes/1e9:.0f}GB {d.interface}"))
            if d.smart:
                s = d.smart
                sections.append(_row("  SMART Health", f"{s.health_percent}% — {s.overall_status}",
                                     warn=s.health_percent < 90, error=s.health_percent < 70 or s.overall_status != "PASSED"))
                if s.nvme:
                    n = s.nvme
                    sections.append(_row("  NVMe Wear", f"{n.percentage_used}% used · Spare: {n.available_spare_percent}%",
                                         warn=n.percentage_used > 70, error=n.percentage_used > 90))
                    if n.media_errors:
                        sections.append(_row("  Media Errors", str(n.media_errors), error=True))
        sections.append("</table>")

        sections.append("</body></html>")
        return "".join(sections)

    def report_json(self, snap: HardwareSnapshot) -> str:
        return json.dumps(snap.to_dict(), indent=2)


# ── Prometheus metrics integration ─────────────────────────────────────────────

def collect_hwinfo_prometheus(snap: HardwareSnapshot, lb: str = "") -> str:
    """
    Return Prometheus text format metrics for all hardware sensors.
    Extends prometheus_metrics.py with deep hardware data.

    Call from prometheus_metrics.collect_soft() to include hardware sensors.
    """
    def _g(name: str, help_: str, val: float, extra_labels: str = "") -> str:
        labels = f"{lb},{extra_labels}" if lb and extra_labels else lb or extra_labels
        labels_str = f"{{{labels}}}" if labels else ""
        return f"# HELP {name} {help_}\n# TYPE {name} gauge\n{name}{labels_str} {val}\n"

    lines: list[str] = []
    c = snap.cpu

    # Per-core temps and clocks
    for core in c.cores:
        if core.temperature_c:
            lines.append(_g("ozma_cpu_core_temperature_celsius", "CPU per-core temperature",
                            core.temperature_c, f'core="{core.index}"'))
        if core.clock_mhz:
            lines.append(_g("ozma_cpu_core_clock_mhz", "CPU per-core clock",
                            core.clock_mhz, f'core="{core.index}"'))

    # CPU package sensors
    if c.package_temp_c:
        lines.append(_g("ozma_cpu_package_temperature_celsius", "CPU package temperature", c.package_temp_c))
    if c.package_power_w:
        lines.append(_g("ozma_cpu_package_power_watts", "CPU package power (RAPL)", c.package_power_w))
    if c.core_power_w:
        lines.append(_g("ozma_cpu_core_power_watts", "CPU core power (RAPL)", c.core_power_w))
    if c.dram_power_w:
        lines.append(_g("ozma_cpu_dram_power_watts", "DRAM power (RAPL)", c.dram_power_w))
    if c.core_voltage_v:
        lines.append(_g("ozma_cpu_vcore_volts", "CPU core voltage", c.core_voltage_v))

    # GPU sensors (extended)
    for gpu in snap.gpus:
        glb = f'gpu="{gpu.index}",model="{gpu.model}"'
        if gpu.temperature_c:
            lines.append(_g("ozma_gpu_temperature_celsius", "GPU temperature", gpu.temperature_c, glb))
        if gpu.hotspot_temp_c:
            lines.append(_g("ozma_gpu_hotspot_temperature_celsius", "GPU hot-spot temperature", gpu.hotspot_temp_c, glb))
        if gpu.vram_temp_c:
            lines.append(_g("ozma_gpu_vram_temperature_celsius", "GPU VRAM temperature", gpu.vram_temp_c, glb))
        if gpu.core_clock_mhz:
            lines.append(_g("ozma_gpu_core_clock_mhz", "GPU core clock", gpu.core_clock_mhz, glb))
        if gpu.memory_clock_mhz:
            lines.append(_g("ozma_gpu_memory_clock_mhz", "GPU memory clock", gpu.memory_clock_mhz, glb))
        if gpu.power_w:
            lines.append(_g("ozma_gpu_power_watts", "GPU power draw", gpu.power_w, glb))
        if gpu.fan_rpm:
            lines.append(_g("ozma_gpu_fan_rpm", "GPU fan speed (RPM)", gpu.fan_rpm, glb))
        if gpu.vram_used_gb:
            lines.append(_g("ozma_gpu_vram_used_gigabytes", "GPU VRAM used", gpu.vram_used_gb, glb))

    # NVMe wear
    for d in snap.storage:
        if d.smart and d.smart.nvme:
            dlb = f'device="{d.device}",model="{d.model}"'
            n = d.smart.nvme
            lines.append(_g("ozma_storage_nvme_percentage_used", "NVMe wear level (% used)", n.percentage_used, dlb))
            lines.append(_g("ozma_storage_nvme_available_spare_percent", "NVMe available spare %", n.available_spare_percent, dlb))
            lines.append(_g("ozma_storage_nvme_media_errors_total", "NVMe media errors", n.media_errors, dlb))
            lines.append(_g("ozma_storage_smart_health_percent", "Storage SMART health %", d.smart.health_percent, dlb))

    # Fans
    for fan in snap.fans:
        flb = f'fan="{fan.name}"'
        lines.append(_g("ozma_fan_rpm", "Fan speed (RPM)", fan.rpm, flb))

    # Voltages
    for v in snap.voltages:
        vlb = f'rail="{v.name}"'
        lines.append(_g("ozma_voltage_volts", "Voltage rail reading", v.voltage_v, vlb))

    # Motherboard temps
    mb = snap.motherboard
    if mb.vrm_temp_c:
        lines.append(_g("ozma_motherboard_vrm_temperature_celsius", "Motherboard VRM temperature", mb.vrm_temp_c))
    if mb.pch_temp_c:
        lines.append(_g("ozma_motherboard_pch_temperature_celsius", "Motherboard PCH temperature", mb.pch_temp_c))
    if mb.ambient_temp_c:
        lines.append(_g("ozma_motherboard_ambient_temperature_celsius", "Motherboard ambient temperature", mb.ambient_temp_c))

    # Battery
    for bat in snap.batteries:
        blb = f'battery="{bat.name}"'
        lines.append(_g("ozma_battery_health_percent", "Battery health %", bat.health_percent, blb))
        lines.append(_g("ozma_battery_charge_percent", "Battery charge %", bat.charge_percent, blb))
        if bat.charge_rate_mw:
            lines.append(_g("ozma_battery_power_milliwatts", "Battery charge/discharge rate mW",
                            bat.charge_rate_mw, blb))

    return "".join(lines)


# ── Sensor poller ──────────────────────────────────────────────────────────────

class SensorPoller:
    """
    Poll hardware sensors at a configurable interval and maintain min/max/avg.

    Integrates with prometheus_metrics.py: call collect() to get the latest
    snapshot for Prometheus scraping.

    Default interval: 1000ms (matches HWiNFO64 default polling period).
    """

    def __init__(self, interval_ms: int = 1000) -> None:
        self._interval = interval_ms / 1000.0
        self._collector = HardwareInfoCollector()
        self._latest: HardwareSnapshot | None = None
        self._stop = asyncio.Event()

    async def run(self) -> None:
        """Poll sensors continuously. Run as asyncio task."""
        while not self._stop.is_set():
            try:
                self._latest = await self._collector.async_snapshot()
            except Exception as e:
                log.debug("Hardware sensor poll error: %s", e)
            await asyncio.sleep(self._interval)

    def stop(self) -> None:
        self._stop.set()

    def latest(self) -> HardwareSnapshot | None:
        return self._latest

    def prometheus_metrics(self, lb: str = "") -> str:
        """Return Prometheus text metrics for the latest snapshot."""
        if not self._latest:
            return ""
        return collect_hwinfo_prometheus(self._latest, lb)

    def report(self, fmt: str = "text") -> str:
        """Generate a hardware report in the requested format."""
        if not self._latest:
            return "No hardware data collected yet."
        c = self._collector
        match fmt:
            case "text":
                return c.report_text(self._latest)
            case "json":
                return c.report_json(self._latest)
            case "csv":
                return c.report_csv(self._latest)
            case "html":
                return c.report_html(self._latest)
            case _:
                return c.report_text(self._latest)
