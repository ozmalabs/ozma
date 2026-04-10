# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Prometheus metrics for soft nodes.

Exposes the full machine picture: hardware inventory (CPU model, RAM
sticks, disks, NICs, GPU) as info metrics, plus live state (usage,
temps, throughput) as gauges/counters. Prometheus scrapes this once
and you see exactly what the machine is and how it's doing.
"""

from __future__ import annotations

import os
import platform
import re
import subprocess
from pathlib import Path
from typing import Any


def _g(name: str, help_text: str, value: Any, labels: str = "") -> str:
    return f"# HELP {name} {help_text}\n# TYPE {name} gauge\n{name}{{{labels}}} {value}\n"


def _c(name: str, help_text: str, value: Any, labels: str = "") -> str:
    return f"# HELP {name} {help_text}\n# TYPE {name} counter\n{name}{{{labels}}} {value}\n"


def _info(name: str, help_text: str, labels: str) -> str:
    """Prometheus info metric (gauge with value 1, data in labels)."""
    return f"# HELP {name} {help_text}\n# TYPE {name} gauge\n{name}{{{labels}}} 1\n"


# ── Hardware inventory (collected once, cached) ────────────────────────────

_hw_cache: dict[str, str] = {}


def _collect_hardware_info(lb: str) -> str:
    """Collect static hardware info. Cached after first call."""
    global _hw_cache
    if "result" in _hw_cache:
        return _hw_cache["result"]

    lines: list[str] = []

    # CPU model
    cpu_model = ""
    try:
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.startswith("model name"):
                cpu_model = line.split(":", 1)[1].strip()
                break
    except OSError:
        pass
    if not cpu_model:
        cpu_model = platform.processor() or platform.machine()

    lines.append(_info("ozma_node_cpu_info", "CPU model and architecture",
                        f'{lb},model="{cpu_model}",arch="{platform.machine()}"'))

    # Physical CPU topology
    try:
        cpuinfo = Path("/proc/cpuinfo").read_text()
        physical_ids = set(re.findall(r"physical id\s*:\s*(\d+)", cpuinfo))
        cores_per = re.findall(r"cpu cores\s*:\s*(\d+)", cpuinfo)
        sockets = len(physical_ids) or 1
        cores = int(cores_per[0]) if cores_per else (os.cpu_count() or 1)
        threads = os.cpu_count() or 1
        lines.append(_g("ozma_node_cpu_sockets", "Physical CPU sockets", sockets, lb))
        lines.append(_g("ozma_node_cpu_cores_per_socket", "Cores per socket", cores, lb))
        lines.append(_g("ozma_node_cpu_threads", "Total threads (logical CPUs)", threads, lb))
    except OSError:
        pass

    # Memory DIMMs (from DMI if available)
    try:
        dmi = subprocess.run(
            ["dmidecode", "-t", "memory"], capture_output=True, text=True, timeout=3,
        )
        if dmi.returncode == 0:
            dimm_idx = 0
            for block in dmi.stdout.split("\n\n"):
                if "Size:" in block and "No Module Installed" not in block:
                    size = ""
                    mtype = ""
                    speed = ""
                    locator = ""
                    for line in block.splitlines():
                        line = line.strip()
                        if line.startswith("Size:"):
                            size = line.split(":", 1)[1].strip()
                        elif line.startswith("Type:") and ":" in line:
                            mtype = line.split(":", 1)[1].strip()
                        elif line.startswith("Speed:"):
                            speed = line.split(":", 1)[1].strip()
                        elif line.startswith("Locator:"):
                            locator = line.split(":", 1)[1].strip()
                    if size:
                        dlb = f'{lb},slot="{locator or f"DIMM{dimm_idx}"}",type="{mtype}",speed="{speed}"'
                        # Parse size to bytes
                        size_bytes = 0
                        if "GB" in size:
                            size_bytes = int(re.search(r"\d+", size).group()) * 1_073_741_824
                        elif "MB" in size:
                            size_bytes = int(re.search(r"\d+", size).group()) * 1_048_576
                        lines.append(_g("ozma_node_memory_dimm_bytes", "DIMM size", size_bytes, dlb))
                        dimm_idx += 1
    except (OSError, FileNotFoundError):
        pass  # dmidecode not available or not root

    # Block devices (disks)
    try:
        for dev in sorted(Path("/sys/block").iterdir()):
            name = dev.name
            # Skip loop, ram, dm devices
            if re.match(r"(loop|ram|dm-|sr|fd)", name):
                continue
            size_sectors = int((dev / "size").read_text().strip())
            if size_sectors == 0:
                continue
            size_bytes = size_sectors * 512

            # Try to get model and serial
            model = ""
            serial = ""
            rotational = 1
            try:
                model_file = dev / "device" / "model"
                if model_file.exists():
                    model = model_file.read_text().strip()
            except OSError:
                pass
            try:
                serial_file = dev / "device" / "serial"
                if serial_file.exists():
                    serial = serial_file.read_text().strip()
            except OSError:
                pass
            try:
                rot_file = dev / "queue" / "rotational"
                if rot_file.exists():
                    rotational = int(rot_file.read_text().strip())
            except (OSError, ValueError):
                pass

            disk_type = "hdd" if rotational else "ssd"
            dlb = f'{lb},device="{name}",model="{model}",type="{disk_type}"'
            lines.append(_g("ozma_node_disk_size_bytes", "Block device total size", size_bytes, dlb))

            # SMART temperature (if smartctl available)
            try:
                smart = subprocess.run(
                    ["smartctl", "-A", f"/dev/{name}"], capture_output=True, text=True, timeout=3,
                )
                for sline in smart.stdout.splitlines():
                    if "Temperature_Celsius" in sline or "Airflow_Temperature" in sline:
                        parts = sline.split()
                        temp = int(parts[-1]) if parts else 0
                        if 0 < temp < 100:
                            lines.append(_g("ozma_node_disk_temperature_celsius", "Disk temperature", temp, dlb))
                            break
            except (OSError, FileNotFoundError):
                pass

            # Partitions
            for part in sorted(dev.glob(f"{name}*")):
                if part.name == name:
                    continue
                try:
                    psize = int((part / "size").read_text().strip()) * 512
                    if psize > 0:
                        plb = f'{lb},device="{part.name}"'
                        lines.append(_g("ozma_node_partition_size_bytes", "Partition size", psize, plb))
                except (OSError, ValueError):
                    pass
    except OSError:
        pass

    # Mounted filesystems (all real mounts)
    try:
        with open("/proc/mounts") as f:
            for line in f:
                parts = line.split()
                if len(parts) >= 3 and parts[0].startswith("/dev/"):
                    mount = parts[1]
                    fstype = parts[2]
                    try:
                        stat = os.statvfs(mount)
                        total = stat.f_blocks * stat.f_frsize
                        free = stat.f_bavail * stat.f_frsize
                        used = total - free
                        if total > 0:
                            mlb = f'{lb},mount="{mount}",device="{parts[0]}",fstype="{fstype}"'
                            lines.append(_g("ozma_node_filesystem_total_bytes", "Filesystem total", total, mlb))
                            lines.append(_g("ozma_node_filesystem_used_bytes", "Filesystem used", used, mlb))
                            lines.append(_g("ozma_node_filesystem_usage_percent", "Filesystem usage %", f"{used / total * 100:.1f}", mlb))
                    except OSError:
                        pass
    except OSError:
        pass

    # NOTE: NICs are collected live (not cached) — USB adapters, VPNs, etc. come and go.

    # GPU (if /proc/driver/nvidia or /sys/class/drm exists)
    try:
        # NVIDIA
        nvsmi = subprocess.run(
            ["nvidia-smi", "--query-gpu=name,memory.total,driver_version,temperature.gpu,utilization.gpu,power.draw",
             "--format=csv,noheader,nounits"],
            capture_output=True, text=True, timeout=3,
        )
        if nvsmi.returncode == 0:
            for idx, line in enumerate(nvsmi.stdout.strip().splitlines()):
                parts = [p.strip() for p in line.split(",")]
                if len(parts) >= 6:
                    glb = f'{lb},gpu="{idx}",model="{parts[0]}",driver="{parts[2]}"'
                    lines.append(_info("ozma_node_gpu_info", "GPU info", glb))
                    try:
                        lines.append(_g("ozma_node_gpu_memory_total_bytes", "GPU memory total", int(float(parts[1])) * 1_048_576, f'{lb},gpu="{idx}"'))
                        lines.append(_g("ozma_node_gpu_temperature_celsius", "GPU temperature", float(parts[3]), f'{lb},gpu="{idx}"'))
                        lines.append(_g("ozma_node_gpu_utilization_percent", "GPU utilization", float(parts[4]), f'{lb},gpu="{idx}"'))
                        lines.append(_g("ozma_node_gpu_power_watts", "GPU power draw", float(parts[5]), f'{lb},gpu="{idx}"'))
                    except (ValueError, IndexError):
                        pass
    except (OSError, FileNotFoundError):
        pass

    # Motherboard / system info
    try:
        vendor = Path("/sys/class/dmi/id/sys_vendor").read_text().strip()
        product = Path("/sys/class/dmi/id/product_name").read_text().strip()
        bios = Path("/sys/class/dmi/id/bios_version").read_text().strip()
        lines.append(_info("ozma_node_system_info", "System vendor and model",
                            f'{lb},vendor="{vendor}",product="{product}",bios="{bios}"'))
    except OSError:
        pass

    # OS info
    lines.append(_info("ozma_node_os_info", "Operating system",
                        f'{lb},system="{platform.system()}",release="{platform.release()}",distro="{_distro_name()}"'))

    result = "".join(lines)
    _hw_cache["result"] = result
    return result


def _distro_name() -> str:
    """Get Linux distro name from os-release."""
    try:
        for line in Path("/etc/os-release").read_text().splitlines():
            if line.startswith("PRETTY_NAME="):
                return line.split("=", 1)[1].strip('"')
    except OSError:
        pass
    return platform.system()


def _collect_displays(lb: str) -> str:
    """
    Collect connected display info. Live — monitors are plugged/unplugged.

    Sources (tried in order):
      1. DRM sysfs (/sys/class/drm/card*-*) — works without X, Wayland, or root
      2. xrandr — richer info (refresh rate, position) but needs X/Wayland
      3. Capture cards (/dev/video* with V4L2) — HDMI inputs on hardware nodes

    Metrics emitted per display:
      ozma_node_display_info        — name, connector, status labels
      ozma_node_display_connected   — 1/0
      ozma_node_display_width_pixels
      ozma_node_display_height_pixels
      ozma_node_display_refresh_hz
      ozma_node_display_physical_width_mm
      ozma_node_display_physical_height_mm
    """
    lines: list[str] = []

    # ── DRM sysfs (works everywhere, no X needed) ─────────────────────
    drm_dir = Path("/sys/class/drm")
    drm_found = False
    if drm_dir.exists():
        for connector in sorted(drm_dir.iterdir()):
            name = connector.name
            # Only real connectors: card0-HDMI-A-1, card0-DP-1, etc.
            if not ("-" in name and name.startswith("card")):
                continue

            status = ""
            try:
                status = (connector / "status").read_text().strip()
            except OSError:
                continue

            connected = status == "connected"
            connector_type = name.split("-", 1)[1] if "-" in name else name
            dlb = f'{lb},output="{connector_type}"'

            lines.append(_g("ozma_node_display_connected", "Display connected", int(connected), dlb))

            if not connected:
                continue
            drm_found = True

            # Parse EDID for resolution and physical size
            edid_path = connector / "edid"
            if edid_path.exists():
                try:
                    edid = edid_path.read_bytes()
                    if len(edid) >= 128 and edid[:8] == b"\x00\xff\xff\xff\xff\xff\xff\x00":
                        # Physical size (cm → mm) from bytes 21-22
                        phys_w = edid[21] * 10  # cm → mm
                        phys_h = edid[22] * 10
                        if phys_w > 0:
                            lines.append(_g("ozma_node_display_physical_width_mm", "Display width (mm)", phys_w, dlb))
                            lines.append(_g("ozma_node_display_physical_height_mm", "Display height (mm)", phys_h, dlb))

                        # Preferred timing from first detailed descriptor (bytes 54-71)
                        if len(edid) >= 71:
                            pixel_clock = int.from_bytes(edid[54:56], "little")
                            if pixel_clock > 0:  # valid descriptor
                                h_active = edid[56] | ((edid[58] & 0xF0) << 4)
                                v_active = edid[59] | ((edid[61] & 0xF0) << 4)
                                h_blank = edid[57] | ((edid[58] & 0x0F) << 8)
                                v_blank = edid[60] | ((edid[61] & 0x0F) << 8)
                                if h_active > 0 and v_active > 0:
                                    lines.append(_g("ozma_node_display_width_pixels", "Display width (px)", h_active, dlb))
                                    lines.append(_g("ozma_node_display_height_pixels", "Display height (px)", v_active, dlb))
                                    # Calculate refresh rate
                                    total_pixels = (h_active + h_blank) * (v_active + v_blank)
                                    if total_pixels > 0:
                                        refresh = (pixel_clock * 10000) / total_pixels
                                        lines.append(_g("ozma_node_display_refresh_hz", "Display refresh rate (Hz)", f"{refresh:.1f}", dlb))

                        # Monitor name from EDID descriptor blocks
                        for desc_offset in (54, 72, 90, 108):
                            if len(edid) > desc_offset + 17:
                                if edid[desc_offset] == 0 and edid[desc_offset + 3] == 0xFC:
                                    mon_name = edid[desc_offset + 5:desc_offset + 18].decode("ascii", errors="ignore").strip()
                                    if mon_name:
                                        lines.append(_info("ozma_node_display_info", "Display model",
                                                            f'{dlb},name="{mon_name}"'))
                except OSError:
                    pass

            # Modes (current mode from DRM)
            modes_path = connector / "modes"
            if modes_path.exists():
                try:
                    modes = modes_path.read_text().strip().splitlines()
                    if modes:
                        # First mode is preferred/current
                        lines.append(_info("ozma_node_display_modes", "Available display modes",
                                            f'{dlb},preferred="{modes[0]}",count="{len(modes)}"'))
                except OSError:
                    pass

    # ── xrandr fallback (if DRM didn't find anything useful) ──────────
    if not drm_found:
        try:
            xr = subprocess.run(
                ["xrandr", "--current"], capture_output=True, text=True, timeout=3,
            )
            if xr.returncode == 0:
                current_output = ""
                for line in xr.stdout.splitlines():
                    if " connected" in line or " disconnected" in line:
                        parts = line.split()
                        current_output = parts[0]
                        connected = "connected" in parts[1] if len(parts) > 1 else False
                        dlb = f'{lb},output="{current_output}"'
                        lines.append(_g("ozma_node_display_connected", "Display connected", int(connected), dlb))
                    elif current_output and line.strip().startswith(("   ", "\t")):
                        # Mode line: "  1920x1080     60.00*+  59.94"
                        line = line.strip()
                        if "*" in line:  # active mode
                            parts = line.split()
                            if parts and "x" in parts[0]:
                                res = parts[0].split("x")
                                dlb = f'{lb},output="{current_output}"'
                                lines.append(_g("ozma_node_display_width_pixels", "Display width (px)", res[0], dlb))
                                lines.append(_g("ozma_node_display_height_pixels", "Display height (px)", res[1], dlb))
                                # Find the refresh rate (number followed by *)
                                for p in parts[1:]:
                                    if "*" in p:
                                        hz = p.replace("*", "").replace("+", "")
                                        try:
                                            lines.append(_g("ozma_node_display_refresh_hz", "Display refresh rate (Hz)", float(hz), dlb))
                                        except ValueError:
                                            pass
                                        break
                            current_output = ""  # only care about active mode
        except (OSError, FileNotFoundError):
            pass

    # Total connected displays
    display_count = sum(1 for l in "".join(lines).splitlines()
                        if "ozma_node_display_connected" in l and l.rstrip().endswith(" 1"))
    lines.append(_g("ozma_node_displays_connected", "Total connected displays", display_count, lb))

    return "".join(lines)


# ── Live metrics ───────────────────────────────────────────────────────────

def collect_soft(node_name: str, connect_client: Any = None,
                  qmp_connected: bool = False,
                  vm_status: str = "unknown") -> str:
    """Collect all metrics for a soft node: hardware info + live state."""
    lb = f'node="{node_name}"'
    lines: list[str] = []

    # ── Hardware inventory (cached after first call) ───────────────────
    lines.append(_collect_hardware_info(lb))

    # ── Connection state ───────────────────────────────────────────────
    if connect_client:
        s = connect_client.state
        lines.append(_g("ozma_node_controller_rtt_ms", "RTT to controller (ms)", f"{s.controller_rtt_ms:.1f}", lb))
        lines.append(_g("ozma_node_controller_packet_loss", "Packet loss ratio", f"{s.controller_packet_loss:.4f}", lb))
        lines.append(_g("ozma_node_controller_jitter_ms", "Jitter (ms)", f"{s.controller_jitter_ms:.1f}", lb))
        lines.append(_g("ozma_node_relay_rtt_ms", "Relay RTT (ms)", f"{s.relay_rtt_ms:.1f}", lb))
        lines.append(_g("ozma_node_relay_connected", "Relay tunnel up", int(s.relay_connected), lb))
        lines.append(_g("ozma_node_connect_reachable", "Connect API reachable", int(s.connect_reachable), lb))
        lines.append(_c("ozma_node_hid_packets_total", "HID packets received", s.hid_packets_received, lb))
        lines.append(_g("ozma_node_hid_packets_per_second", "HID packet rate", f"{s.hid_packets_per_second:.1f}", lb))

    # ── CPU ────────────────────────────────────────────────────────────
    try:
        load = os.getloadavg()
        lines.append(_g("ozma_node_load_1m", "1-minute load average", f"{load[0]:.2f}", lb))
        lines.append(_g("ozma_node_load_5m", "5-minute load average", f"{load[1]:.2f}", lb))
        lines.append(_g("ozma_node_load_15m", "15-minute load average", f"{load[2]:.2f}", lb))
    except OSError:
        pass

    # CPU frequency
    try:
        for cpu_dir in sorted(Path("/sys/devices/system/cpu").glob("cpu[0-9]*")):
            freq_file = cpu_dir / "cpufreq" / "scaling_cur_freq"
            if freq_file.exists():
                freq_khz = int(freq_file.read_text().strip())
                lines.append(_g("ozma_node_cpu_frequency_mhz", "CPU frequency (MHz)", freq_khz // 1000, f'{lb},cpu="{cpu_dir.name}"'))
    except (OSError, ValueError):
        pass

    # CPU temperature (all thermal zones)
    try:
        for tz in sorted(Path("/sys/class/thermal").glob("thermal_zone*")):
            temp_file = tz / "temp"
            type_file = tz / "type"
            if temp_file.exists():
                temp_c = int(temp_file.read_text().strip()) / 1000
                zone_type = type_file.read_text().strip() if type_file.exists() else tz.name
                lines.append(_g("ozma_node_thermal_celsius", "Thermal zone temperature", f"{temp_c:.1f}",
                                f'{lb},zone="{zone_type}"'))
    except (OSError, ValueError):
        pass

    # ── Memory (live) ──────────────────────────────────────────────────
    try:
        meminfo = Path("/proc/meminfo").read_text()
        mem: dict[str, int] = {}
        for line in meminfo.splitlines():
            parts = line.split()
            if len(parts) >= 2:
                mem[parts[0].rstrip(":")] = int(parts[1]) * 1024
        total = mem.get("MemTotal", 0)
        avail = mem.get("MemAvailable", mem.get("MemFree", 0))
        used = total - avail
        buffers = mem.get("Buffers", 0)
        cached = mem.get("Cached", 0)
        swap_total = mem.get("SwapTotal", 0)
        swap_free = mem.get("SwapFree", 0)
        lines.append(_g("ozma_node_memory_total_bytes", "Total memory", total, lb))
        lines.append(_g("ozma_node_memory_used_bytes", "Used memory", used, lb))
        lines.append(_g("ozma_node_memory_available_bytes", "Available memory", avail, lb))
        lines.append(_g("ozma_node_memory_buffers_bytes", "Buffer memory", buffers, lb))
        lines.append(_g("ozma_node_memory_cached_bytes", "Cached memory", cached, lb))
        lines.append(_g("ozma_node_memory_usage_percent", "Memory usage %", f"{used / max(total, 1) * 100:.1f}", lb))
        lines.append(_g("ozma_node_swap_total_bytes", "Swap total", swap_total, lb))
        lines.append(_g("ozma_node_swap_used_bytes", "Swap used", swap_total - swap_free, lb))
    except OSError:
        pass

    # ── Disk I/O (per device) ──────────────────────────────────────────
    try:
        diskstats = Path("/proc/diskstats").read_text()
        for line in diskstats.splitlines():
            parts = line.split()
            if len(parts) >= 14:
                dev = parts[2]
                if re.match(r"(loop|ram|dm-|sr|fd)", dev):
                    continue
                reads = int(parts[3])
                read_sectors = int(parts[5])
                writes = int(parts[7])
                write_sectors = int(parts[9])
                io_ms = int(parts[12])
                if reads + writes == 0:
                    continue
                dlb = f'{lb},device="{dev}"'
                lines.append(_c("ozma_node_disk_reads_total", "Disk read operations", reads, dlb))
                lines.append(_c("ozma_node_disk_read_bytes_total", "Disk bytes read", read_sectors * 512, dlb))
                lines.append(_c("ozma_node_disk_writes_total", "Disk write operations", writes, dlb))
                lines.append(_c("ozma_node_disk_write_bytes_total", "Disk bytes written", write_sectors * 512, dlb))
                lines.append(_c("ozma_node_disk_io_ms_total", "Disk I/O time (ms)", io_ms, dlb))
    except OSError:
        pass

    # ── Network interfaces + I/O (live — NICs come and go) ───────────
    try:
        net_dir = Path("/sys/class/net")
        for iface in sorted(net_dir.iterdir()):
            name = iface.name
            if name == "lo":
                continue

            # NIC info (driver, MAC, speed, link state)
            mac = ""
            speed = 0
            driver = ""
            operstate = ""
            try:
                mac = (iface / "address").read_text().strip()
            except OSError:
                pass
            try:
                speed = int((iface / "speed").read_text().strip())
            except (OSError, ValueError):
                pass
            try:
                driver_link = iface / "device" / "driver"
                if driver_link.exists():
                    driver = driver_link.resolve().name
            except OSError:
                pass
            try:
                operstate = (iface / "operstate").read_text().strip()
            except OSError:
                pass

            ilb = f'{lb},interface="{name}",mac="{mac}",driver="{driver}"'
            lines.append(_info("ozma_node_nic_info", "Network interface", ilb))
            nlb = f'{lb},interface="{name}"'
            lines.append(_g("ozma_node_nic_speed_mbps", "NIC link speed (Mbps)", max(speed, 0), nlb))
            lines.append(_g("ozma_node_nic_up", "NIC link state", int(operstate == "up"), nlb))

            # I/O counters
            stats_dir = iface / "statistics"
            if not stats_dir.exists():
                continue
            for stat_name, metric in [
                ("rx_bytes", "ozma_node_network_rx_bytes_total"),
                ("tx_bytes", "ozma_node_network_tx_bytes_total"),
                ("rx_packets", "ozma_node_network_rx_packets_total"),
                ("tx_packets", "ozma_node_network_tx_packets_total"),
                ("rx_errors", "ozma_node_network_rx_errors_total"),
                ("tx_errors", "ozma_node_network_tx_errors_total"),
                ("rx_dropped", "ozma_node_network_rx_drops_total"),
                ("tx_dropped", "ozma_node_network_tx_drops_total"),
            ]:
                try:
                    val = int((stats_dir / stat_name).read_text().strip())
                    lines.append(_c(metric, f"Network {stat_name}", val, nlb))
                except (OSError, ValueError):
                    pass
    except OSError:
        pass

    # ── Uptime ─────────────────────────────────────────────────────────
    try:
        uptime = float(Path("/proc/uptime").read_text().split()[0])
        lines.append(_c("ozma_node_uptime_seconds", "System uptime", f"{uptime:.0f}", lb))
    except (OSError, ValueError):
        pass

    # ── Process count ──────────────────────────────────────────────────
    try:
        procs = len([p for p in Path("/proc").iterdir() if p.name.isdigit()])
        lines.append(_g("ozma_node_processes", "Running processes", procs, lb))
    except OSError:
        pass

    # ── Displays (DRM + xrandr — live, monitors come and go) ─────────
    lines.append(_collect_displays(lb))

    # ── QMP / VM state ─────────────────────────────────────────────────
    lines.append(_g("ozma_node_qmp_connected", "QMP socket connected", int(qmp_connected), lb))
    vm_running = 1 if vm_status == "running" else 0
    lines.append(_g("ozma_node_vm_running", "VM running state", vm_running, lb))

    # ── Deep hardware sensors (HWiNFO64 parity) ────────────────────────
    # Per-core temps/clocks, RAPL power, GPU extended sensors, NVMe wear,
    # fan RPMs, voltage rails, motherboard VRM/PCH, battery health.
    try:
        from hardware_info import HardwareInfoCollector
        _hw_collector = HardwareInfoCollector()
        _hw_snap = _hw_collector.snapshot()
        from hardware_info import collect_hwinfo_prometheus
        lines.append(collect_hwinfo_prometheus(_hw_snap, lb))
    except Exception:
        pass  # hardware_info not available in all environments

    return "".join(lines)
