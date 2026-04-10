# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
#!/usr/bin/env python3
"""
Ozma Storage Manager — ZFS, mdraid, SAS enclosures, multipath, slow I/O.

Automates the full drive replacement lifecycle:

  1. Monitor  — poll pool/array health; detect DEGRADED/FAULTED/slow I/O
  2. Identify — resolve failed device → serial number → physical slot
  3. Mark     — light enclosure fault LED; log slot + serial for operator
  4. Detect   — watch for new drive insertion (udev events + polling)
  5. Dual-path — if same serial appears on 2 paths → build multipath device first
  6. Validate  — new device (or multipath device) capacity >= failed device
  7. Replace  — zpool replace / mdadm --add using the validated device path
  8. Monitor  — track resilver/sync progress; notify on completion
  9. Complete — extinguish LED; record event; return to HEALTHY

Subsystems:
  ZFS       — zpool status/events/iostat-l; resilver tracking; ZED hooks
  mdraid    — /proc/mdstat; mdadm --detail/--add/--remove; sync progress
  Multipath — dm-multipath (Linux) + gmultipath (FreeBSD); auto-detect dual paths
  Enclosure — SES LEDs via sg_ses / sysfs / sesutil (FreeBSD); slot → device map
  Slow I/O  — ZFS latency percentiles (zpool iostat -l); iostat generic baseline

Cross-platform: Linux (primary), FreeBSD (ZFS + gmultipath + sesutil/camcontrol).

NOTE: Uses stdlib subprocess only (never aiohttp on Windows).
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import platform
import re
import time
import urllib.request
from dataclasses import dataclass, field
from enum import Enum, auto
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.storage_manager")

_IS_FREEBSD = platform.system() == "FreeBSD"
_IS_LINUX = platform.system() == "Linux"


# ── Utilities ──────────────────────────────────────────────────────────────────

async def _run(cmd: list[str], timeout: float = 30.0) -> tuple[str, str, int]:
    """Run a command; return (stdout, stderr, returncode)."""
    try:
        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        stdout, stderr = await asyncio.wait_for(proc.communicate(), timeout=timeout)
        return (
            stdout.decode("utf-8", errors="replace"),
            stderr.decode("utf-8", errors="replace"),
            proc.returncode or 0,
        )
    except asyncio.TimeoutError:
        return "", f"timeout after {timeout}s", -1
    except FileNotFoundError as e:
        return "", str(e), -127


async def _run_ok(cmd: list[str], timeout: float = 30.0) -> str:
    """Run and return stdout; empty string on failure."""
    out, _, rc = await _run(cmd, timeout)
    return out if rc == 0 else ""


# ── Data classes ───────────────────────────────────────────────────────────────

class VdevState(str, Enum):
    ONLINE    = "ONLINE"
    DEGRADED  = "DEGRADED"
    FAULTED   = "FAULTED"
    OFFLINE   = "OFFLINE"
    REMOVED   = "REMOVED"
    UNAVAIL   = "UNAVAIL"
    UNKNOWN   = "UNKNOWN"


@dataclass
class VdevInfo:
    name: str                       # /dev/sda, /dev/disk/by-id/..., gptid/..., etc.
    state: VdevState = VdevState.ONLINE
    read_errors: int = 0
    write_errors: int = 0
    cksum_errors: int = 0
    serial: str = ""                # resolved serial number
    capacity_bytes: int = 0
    slot_index: int = -1            # enclosure slot (-1 = unknown)
    enclosure_device: str = ""      # /dev/sgN or encN (FreeBSD)
    multipath_device: str = ""      # /dev/mapper/mpathX if via multipath
    physical_paths: list[str] = field(default_factory=list)  # raw paths if multipath

    @property
    def failed(self) -> bool:
        return self.state in (VdevState.FAULTED, VdevState.REMOVED, VdevState.UNAVAIL)

    @property
    def has_errors(self) -> bool:
        return self.read_errors + self.write_errors + self.cksum_errors > 0


@dataclass
class PoolStatus:
    name: str
    state: str = "ONLINE"          # ONLINE, DEGRADED, FAULTED, UNAVAIL, REMOVED
    status: str = ""               # human description (e.g. "One or more devices...")
    action: str = ""               # recommended action
    scan: str = ""                 # last scan / resilver line
    resilver_percent: float = 0.0
    resilver_active: bool = False
    vdevs: list[VdevInfo] = field(default_factory=list)
    read_latency_us: float = 0.0   # p99 read latency µs from zpool iostat -l
    write_latency_us: float = 0.0

    @property
    def degraded(self) -> bool:
        return self.state in ("DEGRADED", "FAULTED", "UNAVAIL")

    @property
    def failed_vdevs(self) -> list[VdevInfo]:
        return [v for v in self.vdevs if v.failed]


@dataclass
class MdMember:
    device: str                    # /dev/sda1, etc.
    state: str = "active"          # active, faulty, spare, rebuilding, removed
    serial: str = ""
    capacity_bytes: int = 0
    slot_index: int = -1


@dataclass
class MdArrayStatus:
    device: str                    # /dev/md0
    state: str = "clean"           # clean, degraded, inactive, recovering, resync
    level: str = ""                # raid1, raid5, raid6, raid10, linear
    chunk_kb: int = 0
    members: list[MdMember] = field(default_factory=list)
    sync_percent: float = 0.0
    sync_speed_kbs: int = 0
    sync_active: bool = False
    size_bytes: int = 0

    @property
    def degraded(self) -> bool:
        return self.state in ("degraded", "inactive", "recovering")

    @property
    def failed_members(self) -> list[MdMember]:
        return [m for m in self.members if m.state == "faulty"]


@dataclass
class DriveSlot:
    """A physical slot in a SAS/SATA enclosure."""
    slot_index: int
    enclosure_device: str          # /dev/sgN (Linux) or encN (FreeBSD)
    enclosure_name: str = ""
    device_path: str = ""          # /dev/sdX currently in this slot
    serial: str = ""
    fault_led: bool = False
    locate_led: bool = False


@dataclass
class MultipathDevice:
    """A dm-multipath (Linux) or gmultipath (FreeBSD) device."""
    name: str                      # mpathA, ozma_WD1234, etc.
    device_path: str               # /dev/mapper/mpathA, /dev/multipath/name
    serial: str
    model: str = ""
    vendor: str = ""
    capacity_bytes: int = 0
    paths: list[str] = field(default_factory=list)   # raw block devices
    path_states: dict[str, str] = field(default_factory=dict)  # dev → active/failed/etc.
    active_paths: int = 0
    failed_paths: int = 0

    @property
    def healthy(self) -> bool:
        return self.active_paths > 0 and self.failed_paths == 0

    @property
    def degraded(self) -> bool:
        return self.active_paths > 0 and self.failed_paths > 0


class ReplacementState(Enum):
    HEALTHY            = auto()
    DEGRADED           = auto()    # pool/array is degraded, need to identify failed drive
    LED_ON             = auto()    # fault LED lit, waiting for operator
    WAITING_NEW_DRIVE  = auto()    # watching for new drive insertion
    DUAL_PATH_DETECTED = auto()    # same serial on a second path; building multipath
    BUILDING_MULTIPATH = auto()    # multipathd/gmultipath setup in progress
    VALIDATING         = auto()    # capacity check, serial confirm
    REPLACING          = auto()    # zpool replace / mdadm --add running
    RESILVERING        = auto()    # resilver/rebuild in progress
    COMPLETE           = auto()    # rebuild done; LED off


@dataclass
class ReplacementWorkflow:
    pool_or_array: str
    failed_device: str
    failed_serial: str = ""
    failed_capacity: int = 0
    slot_index: int = -1
    state: ReplacementState = ReplacementState.DEGRADED
    new_device: str = ""
    new_serial: str = ""
    multipath_device: str = ""
    replacement_path: str = ""     # final path used for replace command
    started_at: float = field(default_factory=time.time)
    replaced_at: float = 0.0
    completed_at: float = 0.0
    error: str = ""


# ── ZFS subsystem ──────────────────────────────────────────────────────────────

class ZfsManager:
    """
    ZFS pool health, event monitoring, drive replacement, and I/O latency.

    Works on both Linux (OpenZFS) and FreeBSD (OpenZFS / native ZFS).
    Uses only the zpool/zfs CLI — no library bindings required.
    """

    async def list_pools(self) -> list[str]:
        out = await _run_ok(["zpool", "list", "-H", "-o", "name"])
        return [l.strip() for l in out.splitlines() if l.strip()]

    async def status(self, pool: str = "") -> list[PoolStatus]:
        """Parse `zpool status` output into structured PoolStatus objects."""
        cmd = ["zpool", "status", "-v"]
        if pool:
            cmd.append(pool)
        out, _, rc = await _run(cmd, timeout=30)
        if rc != 0 and not out:
            return []
        return self._parse_zpool_status(out)

    def _parse_zpool_status(self, text: str) -> list[PoolStatus]:
        pools: list[PoolStatus] = []
        current: PoolStatus | None = None
        in_config = False
        indent_base = 0

        for line in text.splitlines():
            stripped = line.strip()
            if not stripped:
                continue

            # New pool
            m = re.match(r"^\s*pool:\s+(.+)", line)
            if m:
                if current:
                    pools.append(current)
                current = PoolStatus(name=m.group(1).strip())
                in_config = False
                continue
            if current is None:
                continue

            m = re.match(r"^\s*state:\s+(.+)", line)
            if m:
                current.state = m.group(1).strip()
                continue

            m = re.match(r"^\s*status:\s+(.+)", line)
            if m:
                current.status = m.group(1).strip()
                continue

            m = re.match(r"^\s*action:\s+(.+)", line)
            if m:
                current.action = m.group(1).strip()
                continue

            m = re.match(r"^\s*scan:\s+(.+)", line)
            if m:
                current.scan = m.group(1).strip()
                # Parse resilver progress
                rm = re.search(r"resilver in progress.*?(\d+\.\d+)%", current.scan)
                if rm:
                    current.resilver_percent = float(rm.group(1))
                    current.resilver_active = True
                continue

            if re.match(r"^\s*config:", line):
                in_config = True
                continue

            if re.match(r"^\s*errors:", line):
                in_config = False
                continue

            if in_config and stripped:
                # Skip the header line
                if stripped.startswith("NAME") and "STATE" in stripped:
                    continue
                # Parse vdev lines: "  sda  ONLINE  0  0  0" etc.
                parts = stripped.split()
                if len(parts) >= 2:
                    dev_name = parts[0]
                    state_str = parts[1].upper()
                    # Skip pool name itself and topology grouping words
                    if dev_name == current.name:
                        continue
                    if dev_name in ("mirror", "raidz", "raidz1", "raidz2", "raidz3",
                                    "draid", "cache", "log", "special", "spare",
                                    "replacing", "spares"):
                        continue
                    try:
                        vstate = VdevState(state_str)
                    except ValueError:
                        vstate = VdevState.UNKNOWN

                    vdev = VdevInfo(name=dev_name, state=vstate)
                    if len(parts) >= 5:
                        try:
                            vdev.read_errors = int(parts[2]) if parts[2] != "-" else 0
                            vdev.write_errors = int(parts[3]) if parts[3] != "-" else 0
                            vdev.cksum_errors = int(parts[4]) if parts[4] != "-" else 0
                        except (ValueError, IndexError):
                            pass
                    current.vdevs.append(vdev)

        if current:
            pools.append(current)
        return pools

    async def resolve_vdev_serial(self, vdev: VdevInfo) -> str:
        """Resolve a vdev device name to a serial number via smartctl."""
        dev_path = self._resolve_device_path(vdev.name)
        if not dev_path:
            return ""
        try:
            from hardware_info import LinuxHardwareCollector, MacOSHardwareCollector
            # Use smartctl -i for just the identity section
            out = await _run_ok(
                ["smartctl", "-i", "--json=c", dev_path], timeout=10
            )
            if out:
                data = json.loads(out)
                return str(data.get("serial_number", "")).strip()
        except Exception:
            pass
        return ""

    def _resolve_device_path(self, name: str) -> str:
        """Resolve a ZFS vdev name to an actual block device path."""
        # Already a /dev/... path
        if name.startswith("/dev/"):
            return name
        # by-id symlink
        by_id = Path(f"/dev/disk/by-id/{name}")
        if by_id.exists():
            return str(by_id)
        # FreeBSD gptid, gpt/*
        if _IS_FREEBSD:
            # /dev/gptid/uuid or /dev/da0 etc
            if "/" not in name:
                p = Path(f"/dev/{name}")
                if p.exists():
                    return str(p)
        # Short names (sda, sdb, nvme0n1)
        p = Path(f"/dev/{name}")
        if p.exists():
            return str(p)
        # Check symlinks in /dev/disk/by-id/ matching suffix
        by_id_dir = Path("/dev/disk/by-id")
        if by_id_dir.exists():
            for link in by_id_dir.iterdir():
                if link.name.endswith(name):
                    return str(link)
        return ""

    async def get_io_latency(self, pool: str) -> tuple[float, float]:
        """
        Return (read_p99_us, write_p99_us) from `zpool iostat -l`.

        zpool iostat -l shows a min/mean/max latency histogram breakdown.
        We return the max read/write latency as a conservative upper bound.
        """
        out = await _run_ok(
            ["zpool", "iostat", "-l", pool, "1", "2"],
            timeout=15,
        )
        if not out:
            return 0.0, 0.0

        read_us = write_us = 0.0
        # iostat -l output includes latency histograms per pool
        # Parse the summary line: pool  r/s  w/s  rMB/s  wMB/s  r_lat  w_lat
        for line in out.splitlines():
            parts = line.split()
            if len(parts) >= 7 and parts[0] == pool:
                try:
                    # Latencies are in ns format: "123us", "1.2ms", "456ns"
                    read_us = self._parse_latency_ns(parts[5]) / 1000
                    write_us = self._parse_latency_ns(parts[6]) / 1000
                except (IndexError, ValueError):
                    pass
        return read_us, write_us

    def _parse_latency_ns(self, s: str) -> float:
        """Parse '123ns', '1.5us', '2ms' into nanoseconds."""
        s = s.strip()
        if s.endswith("ns"):
            return float(s[:-2])
        if s.endswith("us"):
            return float(s[:-2]) * 1000
        if s.endswith("ms"):
            return float(s[:-2]) * 1_000_000
        if s.endswith("s") and not s.endswith("us") and not s.endswith("ms") and not s.endswith("ns"):
            return float(s[:-1]) * 1_000_000_000
        return 0.0

    async def replace_device(self, pool: str, old_device: str,
                              new_device: str) -> tuple[bool, str]:
        """
        Run `zpool replace pool old_device new_device`.

        old_device: the faulted device as shown in zpool status
        new_device: the replacement device path (may be /dev/mapper/mpathX)
        """
        log.info("ZFS replace: pool=%s old=%s new=%s", pool, old_device, new_device)
        _, err, rc = await _run(
            ["zpool", "replace", pool, old_device, new_device],
            timeout=60,
        )
        if rc != 0:
            return False, err.strip()
        return True, ""

    async def resilver_status(self, pool: str) -> tuple[bool, float]:
        """Return (active, percent_complete)."""
        pools = await self.status(pool)
        for p in pools:
            if p.name == pool:
                return p.resilver_active, p.resilver_percent
        return False, 0.0

    async def clear_errors(self, pool: str) -> bool:
        _, _, rc = await _run(["zpool", "clear", pool], timeout=15)
        return rc == 0

    async def scrub(self, pool: str) -> bool:
        _, _, rc = await _run(["zpool", "scrub", pool], timeout=15)
        return rc == 0

    async def watch_events(self, callback: Any) -> None:
        """
        Stream ZFS events via `zpool events -v -H` and call callback(event_dict).

        ZED (ZFS Event Daemon) handles automated responses; this is for
        real-time monitoring and UI updates within the Ozma agent.
        """
        proc = await asyncio.create_subprocess_exec(
            "zpool", "events", "-v", "-H",
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.DEVNULL,
        )
        current_event: dict[str, str] = {}
        while True:
            line_bytes = await proc.stdout.readline()
            if not line_bytes:
                break
            line = line_bytes.decode("utf-8", errors="replace").rstrip()
            if not line:
                if current_event:
                    await callback(current_event)
                    current_event = {}
                continue
            m = re.match(r"^\s+(\S+)\s*=\s*\"?([^\"]+)\"?", line)
            if m:
                current_event[m.group(1)] = m.group(2)


# ── mdraid subsystem ───────────────────────────────────────────────────────────

class MdRaidManager:
    """
    Linux md-raid health monitoring and drive replacement.

    Parses /proc/mdstat and mdadm --detail for live array state.
    Supports hot-add (adding a new/spare drive to an array),
    and tracks sync progress.
    """

    async def list_arrays(self) -> list[str]:
        arrays: list[str] = []
        try:
            mdstat = Path("/proc/mdstat").read_text()
            for m in re.finditer(r"^(md\d+)\s*:", mdstat, re.MULTILINE):
                arrays.append(f"/dev/{m.group(1)}")
        except OSError:
            pass
        return arrays

    async def status(self, array: str = "") -> list[MdArrayStatus]:
        arrays = [array] if array else await self.list_arrays()
        results: list[MdArrayStatus] = []
        for dev in arrays:
            st = await self._array_status(dev)
            if st:
                results.append(st)
        return results

    async def _array_status(self, device: str) -> MdArrayStatus | None:
        out = await _run_ok(["mdadm", "--detail", "--export", device], timeout=10)
        if not out:
            # Fall back to non-export format
            out, _, rc = await _run(["mdadm", "--detail", device], timeout=10)
            if rc != 0:
                return None
            return self._parse_mdadm_detail_verbose(device, out)
        return self._parse_mdadm_export(device, out)

    def _parse_mdadm_export(self, device: str, text: str) -> MdArrayStatus:
        kvs: dict[str, str] = {}
        for line in text.splitlines():
            k, _, v = line.partition("=")
            kvs[k.strip()] = v.strip()

        arr = MdArrayStatus(device=device)
        arr.level = kvs.get("MD_LEVEL", "")
        arr.state = kvs.get("MD_STATE", "unknown").lower()
        size_str = kvs.get("MD_ARRAY_SIZE", "0")
        try:
            arr.size_bytes = int(size_str.split()[0]) * 1024
        except (ValueError, IndexError):
            pass
        # Members via /proc/mdstat (export doesn't include per-member state)
        arr.members = self._parse_members_from_mdstat(device.split("/")[-1])
        # Sync progress
        arr.sync_percent, arr.sync_speed_kbs, arr.sync_active = \
            self._parse_sync_from_mdstat(device.split("/")[-1])
        return arr

    def _parse_mdadm_detail_verbose(self, device: str, text: str) -> MdArrayStatus:
        arr = MdArrayStatus(device=device)
        members: list[MdMember] = []

        state_m = re.search(r"State\s*:\s*(.+)", text)
        if state_m:
            arr.state = state_m.group(1).strip().lower().split(",")[0]

        level_m = re.search(r"Raid Level\s*:\s*(\S+)", text)
        if level_m:
            arr.level = level_m.group(1)

        # Parse device table at bottom of --detail output
        # Format:  "  0  8        0        0  active sync  /dev/sda"
        for m in re.finditer(
            r"^\s*\d+\s+\d+\s+\d+\s+\d+\s+([\w\s]+?)\s+(/dev/\S+)", text, re.MULTILINE
        ):
            state_raw = m.group(1).strip().lower()
            dev = m.group(2).strip()
            member_state = "active"
            if "faulty" in state_raw:
                member_state = "faulty"
            elif "spare" in state_raw:
                member_state = "spare"
            elif "rebuild" in state_raw or "sync" in state_raw:
                member_state = "rebuilding"
            members.append(MdMember(device=dev, state=member_state))

        arr.members = members
        arr.sync_percent, arr.sync_speed_kbs, arr.sync_active = \
            self._parse_sync_from_mdstat(device.split("/")[-1])
        return arr

    def _parse_members_from_mdstat(self, md_name: str) -> list[MdMember]:
        members: list[MdMember] = []
        try:
            mdstat = Path("/proc/mdstat").read_text()
        except OSError:
            return members
        in_array = False
        for line in mdstat.splitlines():
            if line.startswith(md_name + " :"):
                in_array = True
                # Parse member list from this line
                for m in re.finditer(r"(\w+)\[(\d+)\](\(F\)|\(S\))?", line):
                    dev_short = m.group(1)
                    flag = m.group(3) or ""
                    state = "faulty" if "(F)" in flag else "spare" if "(S)" in flag else "active"
                    members.append(MdMember(device=f"/dev/{dev_short}", state=state))
                continue
            if in_array and line.strip():
                break
        return members

    def _parse_sync_from_mdstat(self, md_name: str) -> tuple[float, int, bool]:
        """Return (percent, speed_kbs, active)."""
        try:
            mdstat = Path("/proc/mdstat").read_text()
        except OSError:
            return 0.0, 0, False
        in_array = False
        for line in mdstat.splitlines():
            if line.startswith(md_name + " :"):
                in_array = True
                continue
            if in_array:
                pm = re.search(r"=\s+([\d.]+)%\s+\(\d+/\d+\)", line)
                if pm:
                    pct = float(pm.group(1))
                    sm = re.search(r"speed=(\d+)K/sec", line)
                    speed = int(sm.group(1)) if sm else 0
                    return pct, speed, True
                if line.strip():
                    break
        return 0.0, 0, False

    async def add_device(self, array: str, new_device: str) -> tuple[bool, str]:
        """
        Add a new device to an md array (hot-add / replace failed member).

        For RAID1/5/6/10, adding a device to a degraded array triggers
        automatic rebuild. The device must be at least as large as the
        smallest current member.
        """
        log.info("mdraid add: array=%s device=%s", array, new_device)
        _, err, rc = await _run(
            ["mdadm", "--manage", array, "--add", new_device],
            timeout=30,
        )
        if rc != 0:
            return False, err.strip()
        return True, ""

    async def remove_failed(self, array: str, device: str) -> tuple[bool, str]:
        """Remove a faulty device from an array."""
        _, err, rc = await _run(
            ["mdadm", "--manage", array, "--remove", device],
            timeout=30,
        )
        if rc != 0:
            return False, err.strip()
        return True, ""

    async def resolve_member_serial(self, device: str) -> str:
        """Get the serial number of an md member device."""
        # Strip partition suffix: /dev/sda1 → /dev/sda
        base = re.sub(r"\d+$", "", device)
        out = await _run_ok(
            ["smartctl", "-i", "--json=c", base], timeout=10
        )
        if out:
            try:
                return str(json.loads(out).get("serial_number", "")).strip()
            except json.JSONDecodeError:
                pass
        return ""

    async def get_member_capacity(self, device: str) -> int:
        """Get the usable capacity of a device in bytes."""
        base = re.sub(r"\d+$", "", device)
        out = await _run_ok(
            ["blockdev", "--getsize64", base], timeout=5
        )
        try:
            return int(out.strip())
        except ValueError:
            return 0


# ── Multipath subsystem ────────────────────────────────────────────────────────

class MultipathManager:
    """
    Dual-path auto-detection and device-mapper multipath management.

    Core capability: same serial number appearing on two block devices
    means two I/O paths to the same physical drive (two HBAs, or
    redundant SAS expanders). Before adding the drive to a ZFS/mdraid
    pool, build the multipath device so the pool sees a single logical
    device with automatic path failover.

    Linux:  dm-multipath (multipathd + multipath-tools)
    FreeBSD: gmultipath (GEOM multipath)

    Multipath is ALWAYS preferable to raw device paths when dual paths
    are detected — even for ZFS which does its own error handling. The
    multipath layer provides:
      - Transparent path failover (zero-downtime if one HBA dies)
      - Load balancing across paths
      - A single stable device path for the pool config
    """

    async def scan_block_devices(self) -> dict[str, list[str]]:
        """
        Enumerate all block devices and group by serial number.

        Returns: {serial: ["/dev/sda", "/dev/sdb", ...]}

        Any serial with 2+ entries is a multipath candidate.
        """
        serial_to_devs: dict[str, list[str]] = {}
        block = Path("/sys/block") if _IS_LINUX else None

        if _IS_LINUX and block:
            for dev_path in sorted(block.iterdir()):
                name = dev_path.name
                if re.match(r"(loop|ram|dm-|sr|fd|zram|md)", name):
                    continue
                try:
                    if int((dev_path / "size").read_text().strip()) == 0:
                        continue
                except (OSError, ValueError):
                    continue
                serial = await self._get_serial(f"/dev/{name}")
                if serial:
                    serial_to_devs.setdefault(serial, []).append(f"/dev/{name}")

        elif _IS_FREEBSD:
            # FreeBSD: enumerate /dev/da*, /dev/ada*, /dev/nvd*
            for pattern in ("/dev/da[0-9]*", "/dev/ada[0-9]*"):
                for p in sorted(Path("/dev").glob(pattern[5:])):
                    serial = await self._get_serial(f"/dev/{p.name}")
                    if serial:
                        serial_to_devs.setdefault(serial, []).append(f"/dev/{p.name}")

        return serial_to_devs

    async def _get_serial(self, device: str) -> str:
        """Get serial number for a block device via smartctl."""
        out = await _run_ok(["smartctl", "-i", "--json=c", device], timeout=8)
        if out:
            try:
                data = json.loads(out)
                return str(data.get("serial_number", "")).strip()
            except json.JSONDecodeError:
                pass
        # Fallback: sysfs
        dev_name = device.split("/")[-1]
        for candidate in [
            f"/sys/block/{dev_name}/device/serial",
            f"/sys/class/block/{dev_name}/device/serial",
        ]:
            try:
                return Path(candidate).read_text().strip()
            except OSError:
                pass
        return ""

    async def detect_dual_paths(self) -> list[MultipathDevice]:
        """
        Scan all block devices and return those with 2+ paths to the same drive.

        Also returns already-configured multipath devices so callers can
        distinguish "needs setup" from "already managed".
        """
        serial_map = await self.scan_block_devices()
        dual: list[MultipathDevice] = []

        # Already-managed multipath devices
        existing = {mp.serial: mp for mp in await self.list_multipath_devices()}

        for serial, devs in serial_map.items():
            if len(devs) < 2:
                continue
            if serial in existing:
                # Already managed — return current state
                dual.append(existing[serial])
                continue
            # New dual-path candidate
            mp = MultipathDevice(
                name=f"ozma_{serial[-8:]}",  # last 8 chars of serial as name
                device_path="",              # not yet created
                serial=serial,
                paths=sorted(devs),
                active_paths=len(devs),
            )
            # Get model/vendor/capacity from first path
            out = await _run_ok(
                ["smartctl", "-i", "--json=c", devs[0]], timeout=8
            )
            if out:
                try:
                    data = json.loads(out)
                    mp.model = str(data.get("model_name", "")).strip()
                    mp.vendor = str(data.get("model_family", mp.model)).strip()
                    cap = data.get("user_capacity", {})
                    mp.capacity_bytes = int(cap.get("bytes", 0))
                except (json.JSONDecodeError, TypeError):
                    pass
            dual.append(mp)

        return dual

    async def list_multipath_devices(self) -> list[MultipathDevice]:
        """List currently configured multipath devices."""
        if _IS_LINUX:
            return await self._list_dm_multipath()
        elif _IS_FREEBSD:
            return await self._list_gmultipath()
        return []

    async def _list_dm_multipath(self) -> list[MultipathDevice]:
        """Parse `multipath -ll` output."""
        out = await _run_ok(["multipath", "-ll"], timeout=15)
        if not out:
            return []
        devices: list[MultipathDevice] = []
        current: MultipathDevice | None = None

        for line in out.splitlines():
            # Group header: "mpathA (360000000000000) dm-0 VENDOR,MODEL"
            m = re.match(
                r"^(\w+)\s+\(([0-9a-f]+)\)\s+dm-\d+\s+([^,]+),(.+)", line
            )
            if m:
                if current:
                    devices.append(current)
                name = m.group(1)
                wwid = m.group(2)
                current = MultipathDevice(
                    name=name,
                    device_path=f"/dev/mapper/{name}",
                    serial=wwid,
                    vendor=m.group(3).strip(),
                    model=m.group(4).strip(),
                )
                continue

            if current is None:
                continue

            # Size line: "size=1.8T features='..."
            sm = re.search(r"size=([\d.]+)([KMGTP])", line)
            if sm:
                factor = {"K": 1024, "M": 1024**2, "G": 1024**3,
                          "T": 1024**4, "P": 1024**5}
                current.capacity_bytes = int(
                    float(sm.group(1)) * factor.get(sm.group(2), 1)
                )

            # Path line: "  `- 0:0:0:0 sda 8:0  active ready running"
            pm = re.match(
                r"\s+[`|\\]-\s+\S+\s+(\w+)\s+\S+\s+(\w+)", line
            )
            if pm:
                dev = f"/dev/{pm.group(1)}"
                state = pm.group(2).lower()
                current.paths.append(dev)
                current.path_states[dev] = state
                if state in ("active", "ready"):
                    current.active_paths += 1
                elif state in ("failed", "faulty"):
                    current.failed_paths += 1

        if current:
            devices.append(current)
        return devices

    async def _list_gmultipath(self) -> list[MultipathDevice]:
        """Parse `gmultipath status` on FreeBSD."""
        out = await _run_ok(["gmultipath", "status"], timeout=10)
        devices: list[MultipathDevice] = []
        current: MultipathDevice | None = None
        for line in out.splitlines():
            m = re.match(r"^Name:\s+(\S+)", line)
            if m:
                if current:
                    devices.append(current)
                name = m.group(1)
                current = MultipathDevice(
                    name=name,
                    device_path=f"/dev/multipath/{name}",
                    serial="",
                )
                continue
            if current:
                pm = re.match(r"\s+(/dev/\S+)\s+(\w+)", line)
                if pm:
                    dev, state = pm.group(1), pm.group(2).lower()
                    current.paths.append(dev)
                    current.path_states[dev] = state
                    if state == "active":
                        current.active_paths += 1
                    elif state == "failed":
                        current.failed_paths += 1
        if current:
            devices.append(current)
        return devices

    async def create_multipath(self, mp: MultipathDevice) -> tuple[bool, str]:
        """
        Create a multipath device from the paths in mp.

        Linux:  adds the device to dm-multipath (multipathd must be running)
        FreeBSD: creates a gmultipath device
        """
        if _IS_LINUX:
            return await self._create_dm_multipath(mp)
        elif _IS_FREEBSD:
            return await self._create_gmultipath(mp)
        return False, "unsupported platform"

    async def _create_dm_multipath(self, mp: MultipathDevice) -> tuple[bool, str]:
        """
        Tell multipathd to add the new paths.

        If multipathd is already running (it should be on any SAN-connected
        server), simply calling `multipath` will pick up the new device.
        For each raw path, we first ensure there's no existing device using it.
        """
        log.info("Creating dm-multipath device for serial %s from paths: %s",
                 mp.serial, mp.paths)

        # Ensure multipathd is running
        _, _, rc = await _run(["systemctl", "is-active", "multipathd"], timeout=5)
        if rc != 0:
            # Start it
            _, err, rc2 = await _run(["systemctl", "start", "multipathd"], timeout=15)
            if rc2 != 0:
                return False, f"multipathd start failed: {err}"

        # Add each path to multipath
        for dev in mp.paths:
            await _run(["multipathd", "add", "path", dev], timeout=10)

        # Reload multipath maps
        _, err, rc = await _run(["multipath", "-r"], timeout=15)
        if rc != 0:
            # Not fatal — device may already exist
            log.debug("multipath -r: %s", err.strip())

        # Find the new device
        await asyncio.sleep(1)  # udev settle
        devices = await self._list_dm_multipath()
        for d in devices:
            if any(path in d.paths for path in mp.paths):
                mp.device_path = d.device_path
                mp.name = d.name
                log.info("dm-multipath device created: %s → %s", mp.serial, mp.device_path)
                return True, ""

        return False, "multipath device not found after creation"

    async def _create_gmultipath(self, mp: MultipathDevice) -> tuple[bool, str]:
        """Create a gmultipath device on FreeBSD."""
        name = mp.name or f"ozma_{mp.serial[-8:]}"
        log.info("Creating gmultipath %s from paths: %s", name, mp.paths)
        _, err, rc = await _run(
            ["gmultipath", "label", "-v", name] + mp.paths,
            timeout=30,
        )
        if rc != 0:
            return False, err.strip()
        mp.device_path = f"/dev/multipath/{name}"
        mp.name = name
        return True, ""

    async def remove_multipath(self, name: str) -> bool:
        """Remove a multipath device (after the pool has moved past it)."""
        if _IS_LINUX:
            _, _, rc = await _run(["multipath", "-f", name], timeout=15)
            return rc == 0
        elif _IS_FREEBSD:
            _, _, rc = await _run(["gmultipath", "destroy", name], timeout=15)
            return rc == 0
        return False

    def get_multipath_for_serial(
        self, serial: str, devices: list[MultipathDevice]
    ) -> MultipathDevice | None:
        """Look up a configured multipath device by serial."""
        for d in devices:
            if d.serial == serial or serial in d.serial:
                return d
        return None


# ── SAS/SATA enclosure management ─────────────────────────────────────────────

class EnclosureManager:
    """
    SAS/SATA enclosure LED control and slot → device mapping.

    Supported methods (tried in order):
      1. sysfs  /sys/class/enclosure/*/  (Linux, modern kernels)
      2. sg_ses  (sg3_utils — Linux + FreeBSD)
      3. sesutil  (FreeBSD native)
      4. sas2ircu / storcli  (LSI/Broadcom controllers)
      5. ledctl  (Intel LED management tool)
    """

    async def list_enclosures(self) -> list[str]:
        """List enclosure devices (/dev/sgN on Linux, /dev/ses* on FreeBSD)."""
        devices: list[str] = []
        if _IS_LINUX:
            enc_dir = Path("/sys/class/enclosure")
            if enc_dir.exists():
                for enc in sorted(enc_dir.iterdir()):
                    dev_link = enc / "device"
                    if dev_link.is_symlink():
                        # Find the /dev/sgN for this enclosure
                        sg = await self._enc_to_sg(enc.name)
                        if sg:
                            devices.append(sg)
        elif _IS_FREEBSD:
            for p in sorted(Path("/dev").glob("ses*")):
                devices.append(str(p))
        return devices

    async def _enc_to_sg(self, enc_name: str) -> str:
        """Map enclosure sysfs name to /dev/sgN."""
        try:
            scsi_host = Path(f"/sys/class/enclosure/{enc_name}")
            # Traverse back to find the scsi generic device
            for sg in Path("/sys/class/scsi_generic").iterdir():
                dev_link = sg / "device"
                if dev_link.is_symlink():
                    real = Path(os.readlink(dev_link))
                    if enc_name in str(real):
                        return f"/dev/{sg.name}"
        except OSError:
            pass
        # Fallback: check /dev/sg* for enclosure type
        for sg_dev in sorted(Path("/dev").glob("sg*")):
            out = await _run_ok(
                ["sg_ses", "--page=sn", str(sg_dev)], timeout=3
            )
            if out and "Enclosure" in out:
                return str(sg_dev)
        return ""

    async def list_slots(self, enclosure: str) -> list[DriveSlot]:
        """List all drive slots in an enclosure with current device mapping."""
        if _IS_LINUX:
            slots = self._list_slots_sysfs(enclosure)
            if slots:
                return slots
            return await self._list_slots_sgses(enclosure)
        elif _IS_FREEBSD:
            return await self._list_slots_sesutil(enclosure)
        return []

    def _list_slots_sysfs(self, sg_device: str) -> list[DriveSlot]:
        """Read slot info from /sys/class/enclosure/."""
        slots: list[DriveSlot] = []
        enc_dir = Path("/sys/class/enclosure")
        if not enc_dir.exists():
            return slots
        for enc in sorted(enc_dir.iterdir()):
            for component in sorted(enc.iterdir()):
                # Only slot components (not PSU, fan, temp, etc.)
                ctype_path = component / "type"
                if not ctype_path.exists():
                    continue
                try:
                    ctype = ctype_path.read_text().strip()
                except OSError:
                    continue
                if "drive" not in ctype.lower() and "slot" not in ctype.lower():
                    continue
                try:
                    idx = int(component.name.split(" ")[-1])
                except ValueError:
                    continue
                slot = DriveSlot(
                    slot_index=idx,
                    enclosure_device=sg_device,
                    enclosure_name=enc.name,
                )
                # Device path from slot symlink
                dev_link = component / "device"
                if dev_link.is_symlink():
                    target = os.readlink(dev_link)
                    # Extract block device name
                    m = re.search(r"(sd[a-z]+|nvme\d+n\d+)", target)
                    if m:
                        slot.device_path = f"/dev/{m.group(1)}"
                # LED states
                for led_name, attr in [("fault", "fault_led"), ("locate", "locate_led")]:
                    lp = component / led_name
                    if lp.exists():
                        try:
                            setattr(slot, attr, bool(int(lp.read_text().strip())))
                        except (OSError, ValueError):
                            pass
                slots.append(slot)
        return slots

    async def _list_slots_sgses(self, sg_device: str) -> list[DriveSlot]:
        """Parse `sg_ses --page=asc` for Array/Device slot elements."""
        out = await _run_ok(
            ["sg_ses", "--page=asc", "--inner-hex", sg_device], timeout=10
        )
        slots: list[DriveSlot] = []
        current_slot: DriveSlot | None = None
        for line in out.splitlines():
            m = re.match(r"\s*Array device slot\s+(\d+)", line)
            if m:
                if current_slot:
                    slots.append(current_slot)
                current_slot = DriveSlot(
                    slot_index=int(m.group(1)),
                    enclosure_device=sg_device,
                )
                continue
            if current_slot:
                if "FAULT" in line:
                    current_slot.fault_led = "1" in line.split("FAULT")[-1][:3]
                elif "LOCATE" in line:
                    current_slot.locate_led = "1" in line.split("LOCATE")[-1][:3]
        if current_slot:
            slots.append(current_slot)
        return slots

    async def _list_slots_sesutil(self, ses_device: str) -> list[DriveSlot]:
        """FreeBSD: parse `sesutil encstatus`."""
        out = await _run_ok(["sesutil", "encstatus", ses_device], timeout=10)
        slots: list[DriveSlot] = []
        for m in re.finditer(r"Slot (\d+):\s+(\S+)", out):
            idx = int(m.group(1))
            state = m.group(2)
            slots.append(DriveSlot(
                slot_index=idx,
                enclosure_device=ses_device,
            ))
        return slots

    async def set_fault_led(self, enclosure: str, slot_index: int,
                             on: bool) -> bool:
        """Light or extinguish the fault LED for a drive slot."""
        log.info("Enclosure %s slot %d fault LED: %s",
                 enclosure, slot_index, "ON" if on else "OFF")
        val = "1" if on else "0"

        # Try sysfs first (most reliable on Linux)
        if await self._set_led_sysfs(slot_index, "fault", val):
            return True

        # sg_ses
        if _IS_LINUX:
            _, _, rc = await _run(
                ["sg_ses", "--index", str(slot_index),
                 "--set" if on else "--clear", "fault",
                 enclosure],
                timeout=10,
            )
            if rc == 0:
                return True

        # FreeBSD sesutil
        if _IS_FREEBSD:
            _, _, rc = await _run(
                ["sesutil", "locate" if on else "locate",
                 enclosure, str(slot_index), "on" if on else "off"],
                timeout=10,
            )
            return rc == 0

        # ledctl (Intel)
        if _IS_LINUX:
            dev = await self._slot_to_device(enclosure, slot_index)
            if dev:
                _, _, rc = await _run(
                    ["ledctl", f"failure={dev}" if on else f"normal={dev}"],
                    timeout=10,
                )
                return rc == 0

        return False

    async def set_locate_led(self, enclosure: str, slot_index: int,
                              on: bool) -> bool:
        """Blink the locate LED to identify a drive physically."""
        log.info("Enclosure %s slot %d locate LED: %s",
                 enclosure, slot_index, "ON" if on else "OFF")
        val = "1" if on else "0"
        if await self._set_led_sysfs(slot_index, "locate", val):
            return True
        if _IS_LINUX:
            _, _, rc = await _run(
                ["sg_ses", "--index", str(slot_index),
                 "--set" if on else "--clear", "ident",
                 enclosure],
                timeout=10,
            )
            return rc == 0
        if _IS_FREEBSD:
            _, _, rc = await _run(
                ["sesutil", "locate", enclosure, str(slot_index),
                 "on" if on else "off"],
                timeout=10,
            )
            return rc == 0
        return False

    async def _set_led_sysfs(self, slot_index: int, led: str, val: str) -> bool:
        enc_dir = Path("/sys/class/enclosure")
        if not enc_dir.exists():
            return False
        for enc in sorted(enc_dir.iterdir()):
            for component in sorted(enc.iterdir()):
                try:
                    idx = int(component.name.split(" ")[-1])
                except ValueError:
                    continue
                if idx == slot_index:
                    led_path = component / led
                    if led_path.exists():
                        try:
                            led_path.write_text(val)
                            return True
                        except OSError:
                            pass
        return False

    async def _slot_to_device(self, enclosure: str, slot_index: int) -> str:
        slots = await self.list_slots(enclosure)
        for s in slots:
            if s.slot_index == slot_index:
                return s.device_path
        return ""

    async def map_serial_to_slot(self, serial: str) -> DriveSlot | None:
        """Find which enclosure slot contains the drive with this serial."""
        for enc in await self.list_enclosures():
            for slot in await self.list_slots(enc):
                if not slot.device_path:
                    continue
                dev_serial = await self._get_device_serial(slot.device_path)
                if dev_serial == serial:
                    slot.serial = serial
                    return slot
        return None

    async def _get_device_serial(self, device: str) -> str:
        out = await _run_ok(["smartctl", "-i", "--json=c", device], timeout=8)
        if out:
            try:
                return str(json.loads(out).get("serial_number", "")).strip()
            except json.JSONDecodeError:
                pass
        return ""


# ── Slow I/O detector ──────────────────────────────────────────────────────────

class SlowIODetector:
    """
    Detect abnormally high I/O latency across ZFS, mdraid, and raw block devices.

    Thresholds (configurable):
      ZFS read latency  > 50ms  → WARNING
      ZFS read latency  > 200ms → CRITICAL (imminent failure or rebuild needed)
      ZFS cksum errors  > 0     → WARNING  (data corruption / bad cable)
      mdraid sync       < 1MB/s → WARNING  (very slow rebuild, failing drive)
    """

    def __init__(
        self,
        zfs_read_warn_ms: float = 50.0,
        zfs_read_crit_ms: float = 200.0,
        zfs_write_warn_ms: float = 100.0,
        md_sync_min_mbs: float = 1.0,
    ) -> None:
        self.zfs_read_warn_ms = zfs_read_warn_ms
        self.zfs_read_crit_ms = zfs_read_crit_ms
        self.zfs_write_warn_ms = zfs_write_warn_ms
        self.md_sync_min_mbs = md_sync_min_mbs
        self._zfs = ZfsManager()
        self._md = MdRaidManager()

    async def check_all(self) -> list[dict]:
        """Return list of I/O health events (empty = all good)."""
        events: list[dict] = []
        events.extend(await self._check_zfs_latency())
        events.extend(await self._check_zfs_errors())
        events.extend(await self._check_md_sync())
        return events

    async def _check_zfs_latency(self) -> list[dict]:
        events: list[dict] = []
        pools = await self._zfs.list_pools()
        for pool in pools:
            r_us, w_us = await self._zfs.get_io_latency(pool)
            r_ms = r_us / 1000
            w_ms = w_us / 1000
            if r_ms > self.zfs_read_crit_ms:
                events.append({
                    "type": "SLOW_READ_CRITICAL",
                    "pool": pool,
                    "read_latency_ms": r_ms,
                    "threshold_ms": self.zfs_read_crit_ms,
                    "message": f"ZFS pool '{pool}' read latency {r_ms:.0f}ms (critical threshold: {self.zfs_read_crit_ms}ms)",
                })
            elif r_ms > self.zfs_read_warn_ms:
                events.append({
                    "type": "SLOW_READ_WARNING",
                    "pool": pool,
                    "read_latency_ms": r_ms,
                    "threshold_ms": self.zfs_read_warn_ms,
                    "message": f"ZFS pool '{pool}' read latency {r_ms:.0f}ms (warn threshold: {self.zfs_read_warn_ms}ms)",
                })
            if w_ms > self.zfs_write_warn_ms:
                events.append({
                    "type": "SLOW_WRITE_WARNING",
                    "pool": pool,
                    "write_latency_ms": w_ms,
                    "message": f"ZFS pool '{pool}' write latency {w_ms:.0f}ms",
                })
        return events

    async def _check_zfs_errors(self) -> list[dict]:
        events: list[dict] = []
        for pool_status in await self._zfs.status():
            for vdev in pool_status.vdevs:
                if vdev.cksum_errors > 0:
                    events.append({
                        "type": "ZFS_CKSUM_ERRORS",
                        "pool": pool_status.name,
                        "device": vdev.name,
                        "cksum_errors": vdev.cksum_errors,
                        "message": f"Checksum errors on {vdev.name} in pool '{pool_status.name}': "
                                   f"{vdev.cksum_errors} errors — possible bad cable or failing drive",
                    })
                if vdev.read_errors > 0 or vdev.write_errors > 0:
                    events.append({
                        "type": "ZFS_IO_ERRORS",
                        "pool": pool_status.name,
                        "device": vdev.name,
                        "read_errors": vdev.read_errors,
                        "write_errors": vdev.write_errors,
                        "message": f"I/O errors on {vdev.name}: R={vdev.read_errors} W={vdev.write_errors}",
                    })
        return events

    async def _check_md_sync(self) -> list[dict]:
        events: list[dict] = []
        for arr in await self._md.status():
            if arr.sync_active and arr.sync_speed_kbs > 0:
                speed_mbs = arr.sync_speed_kbs / 1024
                if speed_mbs < self.md_sync_min_mbs:
                    events.append({
                        "type": "MD_SLOW_REBUILD",
                        "array": arr.device,
                        "speed_mbs": speed_mbs,
                        "message": f"mdraid {arr.device} rebuild speed {speed_mbs:.2f}MB/s — "
                                   f"unusually slow, check for failing member",
                    })
        return events


# ── Drive replacement state machine ───────────────────────────────────────────

class ReplacementOrchestrator:
    """
    Full automated drive replacement workflow for ZFS and mdraid.

    State machine:
      DEGRADED
        → Identify failed device + resolve serial
        → Map serial to enclosure slot
        → Light fault LED
        LED_ON
        → Poll for new drive insertion
        WAITING_NEW_DRIVE
        → New drive appears: get serial
        → Check if same serial seen on ANOTHER path (dual-path)
        DUAL_PATH_DETECTED (optional)
        → Build multipath device (multipathd/gmultipath)
        BUILDING_MULTIPATH
        VALIDATING
        → Capacity check (new device >= failed device)
        → Serial confirmation log
        REPLACING
        → zpool replace / mdadm --add
        RESILVERING
        → Poll until resilver/rebuild complete
        COMPLETE
        → Extinguish fault LED
        → Notify controller
        → Record audit event

    Auto-replace: enabled by default. When disabled, workflow pauses at
    VALIDATING and waits for human approval via controller API.
    """

    def __init__(
        self,
        controller_url: str = "",
        auto_replace: bool = True,
        poll_interval_s: float = 5.0,
    ) -> None:
        self._controller_url = controller_url
        self._auto_replace = auto_replace
        self._poll = poll_interval_s
        self._zfs = ZfsManager()
        self._md = MdRaidManager()
        self._mp = MultipathManager()
        self._enc = EnclosureManager()
        self._active: dict[str, ReplacementWorkflow] = {}  # keyed by pool/array name

    async def run(self, stop_event: asyncio.Event) -> None:
        """Main loop: monitor all pools/arrays and drive replacement workflows."""
        log.info("ReplacementOrchestrator starting (auto_replace=%s)", self._auto_replace)
        while not stop_event.is_set():
            await self._monitor_step()
            await asyncio.sleep(self._poll)

    async def _monitor_step(self) -> None:
        """One monitoring iteration."""
        # Check ZFS pools
        for pool_status in await self._zfs.status():
            if pool_status.degraded:
                await self._handle_degraded_zfs(pool_status)
            elif pool_status.name in self._active:
                wf = self._active[pool_status.name]
                if wf.state == ReplacementState.RESILVERING:
                    await self._poll_zfs_resilver(pool_status, wf)

        # Check mdraid arrays
        for arr in await self._md.status():
            if arr.degraded:
                await self._handle_degraded_md(arr)
            elif arr.device in self._active:
                wf = self._active[arr.device]
                if wf.state == ReplacementState.RESILVERING:
                    await self._poll_md_rebuild(arr, wf)

        # Advance WAITING_NEW_DRIVE workflows
        for wf in list(self._active.values()):
            if wf.state == ReplacementState.WAITING_NEW_DRIVE:
                await self._check_for_new_drive(wf)

    # ── ZFS workflow ──────────────────────────────────────────────────────

    async def _handle_degraded_zfs(self, pool: PoolStatus) -> None:
        if pool.name in self._active:
            return  # Already handling
        for vdev in pool.failed_vdevs:
            wf = ReplacementWorkflow(
                pool_or_array=pool.name,
                failed_device=vdev.name,
            )
            # Resolve serial and slot
            wf.failed_serial = await self._zfs.resolve_vdev_serial(vdev)
            wf.failed_capacity = vdev.capacity_bytes
            await self._identify_and_mark(wf)
            self._active[pool.name] = wf
            log.warning("ZFS pool '%s' degraded: failed device %s (serial: %s, slot: %d)",
                        pool.name, vdev.name, wf.failed_serial or "unknown", wf.slot_index)
            await self._notify(wf, "DEGRADED")
            break  # Handle one at a time

    async def _poll_zfs_resilver(self, pool: PoolStatus, wf: ReplacementWorkflow) -> None:
        if not pool.resilver_active:
            wf.state = ReplacementState.COMPLETE
            wf.completed_at = time.time()
            log.info("ZFS resilver complete for pool '%s'", wf.pool_or_array)
            await self._complete_workflow(wf)
        else:
            log.debug("ZFS resilver %.1f%% — pool '%s'",
                      pool.resilver_percent, pool.name)

    # ── mdraid workflow ───────────────────────────────────────────────────

    async def _handle_degraded_md(self, arr: MdArrayStatus) -> None:
        if arr.device in self._active:
            return
        for member in arr.failed_members:
            wf = ReplacementWorkflow(
                pool_or_array=arr.device,
                failed_device=member.device,
            )
            wf.failed_serial = await self._md.resolve_member_serial(member.device)
            wf.failed_capacity = await self._md.get_member_capacity(member.device)
            await self._identify_and_mark(wf)
            self._active[arr.device] = wf
            log.warning("mdraid '%s' degraded: failed member %s (serial: %s)",
                        arr.device, member.device, wf.failed_serial or "unknown")
            await self._notify(wf, "DEGRADED")
            break

    async def _poll_md_rebuild(self, arr: MdArrayStatus, wf: ReplacementWorkflow) -> None:
        if not arr.sync_active:
            wf.state = ReplacementState.COMPLETE
            wf.completed_at = time.time()
            log.info("mdraid rebuild complete for '%s'", wf.pool_or_array)
            await self._complete_workflow(wf)
        else:
            log.debug("mdraid rebuild %.1f%% @ %dKB/s — '%s'",
                      arr.sync_percent, arr.sync_speed_kbs, arr.device)

    # ── Shared workflow steps ─────────────────────────────────────────────

    async def _identify_and_mark(self, wf: ReplacementWorkflow) -> None:
        """Resolve serial → enclosure slot → light fault LED."""
        if wf.failed_serial:
            slot = await self._enc.map_serial_to_slot(wf.failed_serial)
            if slot:
                wf.slot_index = slot.slot_index
                await self._enc.set_fault_led(
                    slot.enclosure_device, slot.slot_index, on=True
                )
                await self._enc.set_locate_led(
                    slot.enclosure_device, slot.slot_index, on=True
                )
                log.info("Fault LED lit: enclosure %s slot %d (serial %s)",
                         slot.enclosure_device, slot.slot_index, wf.failed_serial)
        wf.state = ReplacementState.WAITING_NEW_DRIVE

    async def _check_for_new_drive(self, wf: ReplacementWorkflow) -> None:
        """
        Watch for a new drive to appear. Also detects dual-path scenarios.

        A "new" drive is one whose serial is NOT the failed drive's serial
        AND was not present before the failure. We detect it by scanning
        all current block devices and comparing against known pool members.
        """
        serial_map = await self._mp.scan_block_devices()

        for serial, paths in serial_map.items():
            if serial == wf.failed_serial:
                continue  # Skip the failed drive itself
            if serial in (m.failed_serial for m in self._active.values()
                          if m is not wf):
                continue  # Skip other active failures

            # Is this a known-good pool member already?
            if await self._is_existing_pool_member(wf, serial):
                continue

            # New drive detected
            if len(paths) >= 2:
                # DUAL-PATH: same serial on multiple block devices
                wf.new_serial = serial
                wf.new_device = paths[0]
                wf.state = ReplacementState.DUAL_PATH_DETECTED
                log.info(
                    "Dual-path drive detected: serial %s on paths %s — "
                    "building multipath device before adding to %s",
                    serial, paths, wf.pool_or_array,
                )
                await self._build_multipath_and_replace(wf, serial, paths)
                return
            else:
                # Single-path new drive
                wf.new_device = paths[0]
                wf.new_serial = serial
                log.info("New drive detected: %s (serial %s)",
                         paths[0], serial)
                await self._validate_and_replace(wf, paths[0])
                return

    async def _build_multipath_and_replace(
        self, wf: ReplacementWorkflow, serial: str, paths: list[str]
    ) -> None:
        """
        Build a multipath device from the dual-path drives, then replace
        the failed device with the multipath device.
        """
        wf.state = ReplacementState.BUILDING_MULTIPATH

        # Get capacity of first path
        capacity = 0
        out = await _run_ok(
            ["blockdev", "--getsize64", paths[0]], timeout=5
        )
        try:
            capacity = int(out.strip())
        except ValueError:
            pass

        mp = MultipathDevice(
            name=f"ozma_{serial[-8:]}",
            device_path="",
            serial=serial,
            paths=paths,
            capacity_bytes=capacity,
        )

        ok, err = await self._mp.create_multipath(mp)
        if not ok:
            wf.error = f"Multipath creation failed: {err}"
            wf.state = ReplacementState.WAITING_NEW_DRIVE
            log.error("Multipath creation failed for serial %s: %s", serial, err)
            await self._notify(wf, "MULTIPATH_FAILED")
            return

        wf.multipath_device = mp.device_path
        log.info("Multipath device created: %s (serial %s, paths: %s)",
                 mp.device_path, serial, paths)
        await self._notify(wf, "MULTIPATH_CREATED")
        await self._validate_and_replace(wf, mp.device_path)

    async def _validate_and_replace(
        self, wf: ReplacementWorkflow, replacement_path: str
    ) -> None:
        """Capacity check, serial log, then replace."""
        wf.state = ReplacementState.VALIDATING

        # Capacity check: new device must be >= failed device
        out = await _run_ok(
            ["blockdev", "--getsize64", replacement_path], timeout=5
        )
        try:
            new_capacity = int(out.strip())
        except ValueError:
            new_capacity = 0

        if wf.failed_capacity > 0 and new_capacity < wf.failed_capacity:
            wf.error = (
                f"New device {replacement_path} ({new_capacity} bytes) is smaller "
                f"than failed device ({wf.failed_capacity} bytes) — cannot replace"
            )
            log.error(wf.error)
            wf.state = ReplacementState.WAITING_NEW_DRIVE
            await self._notify(wf, "CAPACITY_MISMATCH")
            return

        wf.replacement_path = replacement_path
        log.info(
            "Replacement validated: %s (serial %s, %d bytes) → replacing %s in %s",
            replacement_path, wf.new_serial, new_capacity,
            wf.failed_device, wf.pool_or_array,
        )

        if not self._auto_replace:
            wf.state = ReplacementState.VALIDATING  # Hold for human approval
            await self._notify(wf, "AWAITING_APPROVAL")
            log.info("Auto-replace disabled — waiting for approval to replace %s",
                     wf.failed_device)
            return

        await self._do_replace(wf)

    async def approve_replacement(self, pool_or_array: str) -> tuple[bool, str]:
        """Manual approval for non-auto-replace mode."""
        wf = self._active.get(pool_or_array)
        if not wf or wf.state != ReplacementState.VALIDATING:
            return False, "No workflow awaiting approval"
        await self._do_replace(wf)
        return True, ""

    async def _do_replace(self, wf: ReplacementWorkflow) -> None:
        """Execute the actual replace command."""
        wf.state = ReplacementState.REPLACING
        wf.replaced_at = time.time()

        if "md" in wf.pool_or_array:
            ok, err = await self._md.add_device(wf.pool_or_array, wf.replacement_path)
        else:
            ok, err = await self._zfs.replace_device(
                wf.pool_or_array, wf.failed_device, wf.replacement_path
            )

        if not ok:
            wf.error = err
            wf.state = ReplacementState.DEGRADED
            log.error("Replace failed for %s: %s", wf.pool_or_array, err)
            await self._notify(wf, "REPLACE_FAILED")
            return

        wf.state = ReplacementState.RESILVERING
        log.info("Replace command issued for %s — resilver/rebuild in progress",
                 wf.pool_or_array)
        await self._notify(wf, "RESILVERING")

    async def _complete_workflow(self, wf: ReplacementWorkflow) -> None:
        """Turn off LEDs, notify, clean up."""
        # Extinguish fault and locate LEDs
        if wf.slot_index >= 0:
            enclosures = await self._enc.list_enclosures()
            for enc in enclosures:
                await self._enc.set_fault_led(enc, wf.slot_index, on=False)
                await self._enc.set_locate_led(enc, wf.slot_index, on=False)

        await self._notify(wf, "COMPLETE")
        del self._active[wf.pool_or_array]
        log.info("Replacement complete: %s → %s in %s (serial: %s)",
                 wf.failed_device, wf.replacement_path, wf.pool_or_array,
                 wf.new_serial)

    async def _is_existing_pool_member(self, wf: ReplacementWorkflow,
                                        serial: str) -> bool:
        """Check if this serial belongs to a healthy existing pool member."""
        for pool_status in await self._zfs.status(
            wf.pool_or_array if "md" not in wf.pool_or_array else ""
        ):
            for vdev in pool_status.vdevs:
                if not vdev.failed:
                    vdev_serial = await self._zfs.resolve_vdev_serial(vdev)
                    if vdev_serial == serial:
                        return True
        return False

    async def _notify(self, wf: ReplacementWorkflow, event: str) -> None:
        """Send event to the controller for dashboard/alert display."""
        if not self._controller_url:
            return
        payload = json.dumps({
            "type": "storage_replacement",
            "event": event,
            "pool": wf.pool_or_array,
            "failed_device": wf.failed_device,
            "failed_serial": wf.failed_serial,
            "new_serial": wf.new_serial,
            "multipath_device": wf.multipath_device,
            "replacement_path": wf.replacement_path,
            "slot_index": wf.slot_index,
            "state": wf.state.name,
            "timestamp": time.time(),
            "error": wf.error,
        }).encode()
        url = f"{self._controller_url.rstrip('/')}/api/v1/events"
        try:
            req = urllib.request.Request(
                url, data=payload,
                headers={"Content-Type": "application/json"},
                method="POST",
            )
            with urllib.request.urlopen(req, timeout=5):
                pass
        except Exception:
            pass

    def status(self) -> list[dict]:
        """Return all active replacement workflows for API/dashboard."""
        return [
            {
                "pool_or_array": wf.pool_or_array,
                "state": wf.state.name,
                "failed_device": wf.failed_device,
                "failed_serial": wf.failed_serial,
                "slot_index": wf.slot_index,
                "new_device": wf.new_device,
                "new_serial": wf.new_serial,
                "multipath_device": wf.multipath_device,
                "replacement_path": wf.replacement_path,
                "started_at": wf.started_at,
                "replaced_at": wf.replaced_at,
                "error": wf.error,
            }
            for wf in self._active.values()
        ]


# ── Top-level manager ──────────────────────────────────────────────────────────

class StorageHealthManager:
    """
    Top-level storage health manager. Wire into the ozma agent.

    Runs three concurrent loops:
      1. ReplacementOrchestrator — drive replacement state machine
      2. SlowIODetector          — latency and error threshold alerting
      3. MultipathManager        — periodic dual-path scan

    All events flow to the controller via /api/v1/events.
    All metrics fed to prometheus_metrics via collect_storage_prometheus().
    """

    def __init__(
        self,
        controller_url: str = "",
        auto_replace: bool = True,
        slow_read_warn_ms: float = 50.0,
        slow_read_crit_ms: float = 200.0,
    ) -> None:
        self._controller_url = controller_url
        self._orchestrator = ReplacementOrchestrator(
            controller_url=controller_url,
            auto_replace=auto_replace,
        )
        self._slow_io = SlowIODetector(
            zfs_read_warn_ms=slow_read_warn_ms,
            zfs_read_crit_ms=slow_read_crit_ms,
        )
        self._mp = MultipathManager()
        self._stop = asyncio.Event()
        self._multipath_cache: list[MultipathDevice] = []
        self._io_events: list[dict] = []

    async def run(self) -> None:
        log.info("StorageHealthManager starting")
        await asyncio.gather(
            self._orchestrator.run(self._stop),
            self._slow_io_loop(),
            self._multipath_scan_loop(),
        )

    async def stop(self) -> None:
        self._stop.set()

    async def _slow_io_loop(self) -> None:
        while not self._stop.is_set():
            try:
                events = await self._slow_io.check_all()
                for ev in events:
                    log.warning("Storage I/O alert: %s", ev["message"])
                    await self._notify_event(ev)
                self._io_events = events
            except Exception as e:
                log.debug("Slow I/O check error: %s", e)
            await asyncio.sleep(30)

    async def _multipath_scan_loop(self) -> None:
        """Periodically scan for new dual-path devices not yet in multipath."""
        while not self._stop.is_set():
            try:
                dual = await self._mp.detect_dual_paths()
                for mp in dual:
                    if not mp.device_path:
                        log.info(
                            "Unmanaged dual-path drive detected: serial %s on %s — "
                            "creating multipath device",
                            mp.serial, mp.paths,
                        )
                        ok, err = await self._mp.create_multipath(mp)
                        if ok:
                            log.info("Created multipath device %s for serial %s",
                                     mp.device_path, mp.serial)
                        else:
                            log.warning("Multipath creation failed for %s: %s",
                                        mp.serial, err)
                self._multipath_cache = dual
            except Exception as e:
                log.debug("Multipath scan error: %s", e)
            await asyncio.sleep(60)

    async def _notify_event(self, event: dict) -> None:
        if not self._controller_url:
            return
        payload = json.dumps(event).encode()
        url = f"{self._controller_url.rstrip('/')}/api/v1/events"
        try:
            req = urllib.request.Request(
                url, data=payload,
                headers={"Content-Type": "application/json"},
                method="POST",
            )
            with urllib.request.urlopen(req, timeout=5):
                pass
        except Exception:
            pass

    def status(self) -> dict:
        return {
            "replacements": self._orchestrator.status(),
            "io_alerts": self._io_events,
            "multipath_devices": [
                {
                    "name": mp.name,
                    "device_path": mp.device_path,
                    "serial": mp.serial,
                    "paths": mp.paths,
                    "active_paths": mp.active_paths,
                    "failed_paths": mp.failed_paths,
                    "capacity_bytes": mp.capacity_bytes,
                }
                for mp in self._multipath_cache
            ],
        }

    async def approve_replacement(self, pool_or_array: str) -> tuple[bool, str]:
        """API endpoint: approve a pending replacement."""
        return await self._orchestrator.approve_replacement(pool_or_array)


def collect_storage_prometheus(manager: StorageHealthManager, lb: str = "") -> str:
    """
    Prometheus metrics for storage health. Call from prometheus_metrics.py.
    """
    def _g(name: str, help_: str, val: float, extra: str = "") -> str:
        labels = f"{lb},{extra}" if lb and extra else lb or extra
        labels_str = f"{{{labels}}}" if labels else ""
        return f"# HELP {name} {help_}\n# TYPE {name} gauge\n{name}{labels_str} {val}\n"

    lines: list[str] = []
    status = manager.status()

    lines.append(_g("ozma_storage_replacements_active",
                    "Number of active drive replacement workflows",
                    len(status["replacements"])))
    lines.append(_g("ozma_storage_io_alerts_active",
                    "Number of active I/O health alerts",
                    len(status["io_alerts"])))

    for mp in status["multipath_devices"]:
        mlb = f'name="{mp["name"]}",serial="{mp["serial"]}"'
        lines.append(_g("ozma_multipath_active_paths",
                        "Active paths in multipath device",
                        mp["active_paths"], mlb))
        lines.append(_g("ozma_multipath_failed_paths",
                        "Failed paths in multipath device",
                        mp["failed_paths"], mlb))

    for ev in status["io_alerts"]:
        if "read_latency_ms" in ev:
            pool_lb = f'pool="{ev.get("pool","")}"'
            lines.append(_g("ozma_storage_zfs_read_latency_ms",
                            "ZFS pool read latency (ms)",
                            ev["read_latency_ms"], pool_lb))

    return "".join(lines)
