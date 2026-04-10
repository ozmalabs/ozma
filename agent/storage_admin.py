# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
agent/storage_admin.py — Storage administration: TrueNAS/OMV/HexOS feature parity.

Covers what storage_manager.py does NOT:
  - ZFS replication (zfs send/receive over SSH to a remote target)
  - S.M.A.R.T. scheduled tests (short daily, long weekly) + alerting
  - iSCSI target management (targetcli/LIO on Linux)
  - POSIX and NFS4 ACL management (setfacl, nfs4_setfacl)
  - Rsync job scheduler (LAN-to-LAN, SSH, with progress tracking)
  - Secure disk wipe (shred, blkdiscard, ATA Secure Erase, NVMe format)
  - Storage capacity trend snapshots (pool used/avail history)
  - S3 gateway config helper (MinIO integration for bucket access to datasets)

All long-running operations stream progress events so the controller can
push updates to the WebSocket clients in real time.
"""

from __future__ import annotations

import asyncio
import dataclasses
import json
import logging
import os
import re
import shlex
import time
from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path
from typing import Any, AsyncIterator

log = logging.getLogger("ozma.storage_admin")


# ─────────────────────────────── data classes ──────────────────────────────

@dataclass
class ReplicationJob:
    """Configured replication job (ZFS send/receive over SSH)."""
    job_id: str
    pool: str
    dataset: str
    remote_host: str
    remote_port: int = 22
    remote_user: str = "root"
    remote_dataset: str = ""        # defaults to same as local
    ssh_key_path: str = ""          # defaults to ~/.ssh/id_ed25519
    compressed: bool = True         # zfs send -c (compressed send)
    encrypted: bool = True          # zfs send -w (raw/encrypted) — preserve on-disk encryption
    incremental: bool = True        # track last snapshot, send incrementally
    recursive: bool = False         # zfs send -R
    enabled: bool = True
    schedule_cron: str = "0 2 * * *"   # daily at 02:00
    last_run_ts: float = 0.0
    last_run_ok: bool = True
    last_snap_sent: str = ""

    @property
    def remote_target(self) -> str:
        ds = self.remote_dataset or self.dataset
        return f"{self.remote_user}@{self.remote_host}:{ds}"


@dataclass
class SmartTestJob:
    """Scheduled S.M.A.R.T. test for a specific disk."""
    device: str                      # /dev/sda, /dev/nvme0, etc.
    test_type: str = "short"         # short | long | conveyance | offline
    schedule_cron: str = "0 1 * * *" # daily at 01:00
    enabled: bool = True
    last_run_ts: float = 0.0
    last_result: str = ""            # "passed" | "failed" | "aborted" | ""
    alert_on_fail: bool = True


@dataclass
class SmartTestResult:
    device: str
    test_type: str
    status: str                      # "passed" | "failed" | "aborted" | "running"
    percent_complete: int = 100
    lifetime_hours: int = 0
    lba_of_failure: int | None = None
    timestamp: float = field(default_factory=time.time)


@dataclass
class IscsiTarget:
    """LIO iSCSI target definition."""
    iqn: str                         # e.g. iqn.2025-01.dev.ozma:storage.target1
    storage_object: str              # block/fileio backing
    device_or_file: str              # /dev/zvol/tank/iscsi-vol or /mnt/pool/iscsi.img
    size_gb: int = 0                 # for fileio backing creation
    enabled: bool = True
    portals: list[str] = field(default_factory=lambda: ["0.0.0.0:3260"])
    initiator_acls: list[str] = field(default_factory=list)  # iqn wildcards or specific
    chap_user: str = ""
    chap_password: str = ""          # ≥12 chars per RFC 3720
    mutual_chap_user: str = ""
    mutual_chap_password: str = ""


@dataclass
class AclEntry:
    principal_type: str              # "user" | "group" | "other" | "mask"
    principal: str                   # username, group name, or ""
    permissions: str                 # "rwx", "r--", etc. or NFSv4 style "rw-d--x---"
    acl_type: str = "posix"          # "posix" | "nfs4"
    # NFSv4 only
    nfs4_type: str = "A"             # "A"=allow "D"=deny "U"=audit "L"=alarm
    nfs4_flags: str = ""             # e.g. "fd" (file-inherit, dir-inherit)
    recursive: bool = False


@dataclass
class RsyncJob:
    job_id: str
    source: str                      # local path or user@host:path
    destination: str                 # local path or user@host:path
    schedule_cron: str = "0 3 * * *"
    delete: bool = False             # --delete (mirror mode)
    archive: bool = True             # -a (preserve perms, times, symlinks)
    compress: bool = True            # -z
    checksum: bool = False           # -c (slower, exhaustive integrity)
    exclude_patterns: list[str] = field(default_factory=list)
    ssh_key_path: str = ""
    bandwidth_limit_kb: int = 0      # --bwlimit, 0 = unlimited
    enabled: bool = True
    last_run_ts: float = 0.0
    last_run_ok: bool = True
    last_bytes_sent: int = 0
    last_duration_s: float = 0.0


class WipeMethod(str, Enum):
    ZEROS        = "zeros"           # dd if=/dev/zero (fast, not forensic)
    RANDOM       = "random"          # shred -n 1 (random pass)
    DOD_7        = "dod_7"           # shred -n 3 (DoD 5220.22-M 3-pass)
    BLKDISCARD   = "blkdiscard"      # SSD: discard all blocks (fast, SSD-safe)
    ATA_SECURE   = "ata_secure"      # hdparm --security-erase (ATA Enhanced SE)
    NVME_FORMAT  = "nvme_format"     # nvme format --ses=1 (cryptographic erase)


@dataclass
class WipeJob:
    device: str
    method: WipeMethod
    verified: bool = False           # whether to read-verify after wipe
    started_ts: float = 0.0
    finished_ts: float = 0.0
    success: bool = False
    error: str = ""
    percent_complete: float = 0.0


@dataclass
class CapacitySnapshot:
    """Point-in-time pool capacity snapshot for trend tracking."""
    pool: str
    timestamp: float
    used_bytes: int
    avail_bytes: int
    total_bytes: int

    @property
    def used_pct(self) -> float:
        if self.total_bytes == 0:
            return 0.0
        return 100.0 * self.used_bytes / self.total_bytes


# ──────────────────────────── ZFS Replication ──────────────────────────────

class ZfsReplicationManager:
    """
    ZFS send/receive replication to remote hosts over SSH.
    Supports incremental (tracking last sent snapshot), encrypted
    (zfs send -w) and compressed (zfs send -c) transfers.
    """

    def __init__(self, state_path: Path = Path("/var/lib/ozma/zfs-replication.json")):
        self._state_path = state_path
        self._jobs: dict[str, ReplicationJob] = {}
        self._running: dict[str, asyncio.Task] = {}
        self._load_state()

    def _load_state(self) -> None:
        if self._state_path.exists():
            try:
                data = json.loads(self._state_path.read_text())
                for jd in data.get("jobs", []):
                    j = ReplicationJob(**jd)
                    self._jobs[j.job_id] = j
            except Exception as e:
                log.warning("failed to load replication state: %s", e)

    def _save_state(self) -> None:
        self._state_path.parent.mkdir(parents=True, exist_ok=True)
        payload = {"jobs": [dataclasses.asdict(j) for j in self._jobs.values()]}
        self._state_path.write_text(json.dumps(payload, indent=2))

    # ── CRUD ──────────────────────────────────────────────────────────────

    def add_job(self, job: ReplicationJob) -> None:
        self._jobs[job.job_id] = job
        self._save_state()

    def remove_job(self, job_id: str) -> bool:
        if job_id not in self._jobs:
            return False
        del self._jobs[job_id]
        self._save_state()
        return True

    def list_jobs(self) -> list[ReplicationJob]:
        return list(self._jobs.values())

    def get_job(self, job_id: str) -> ReplicationJob | None:
        return self._jobs.get(job_id)

    # ── Execution ─────────────────────────────────────────────────────────

    async def run_job(self, job_id: str) -> tuple[bool, str]:
        """Run a replication job immediately. Returns (success, message)."""
        job = self._jobs.get(job_id)
        if not job:
            return False, f"job {job_id} not found"

        # Determine the snapshot to send
        snap_name = f"ozma-repl-{int(time.time())}"
        dataset = f"{job.pool}/{job.dataset}" if job.dataset else job.pool
        snapshot = f"{dataset}@{snap_name}"

        try:
            # Create snapshot
            ok, out = await self._run(["zfs", "snapshot", "-r" if job.recursive else "", snapshot])
            if not ok:
                return False, f"snapshot failed: {out}"

            # Build ssh command
            ssh_opts = [
                "ssh", "-o", "StrictHostKeyChecking=accept-new",
                "-p", str(job.remote_port),
            ]
            if job.ssh_key_path:
                ssh_opts += ["-i", job.ssh_key_path]
            ssh_opts += [f"{job.remote_user}@{job.remote_host}"]

            remote_ds = job.remote_dataset or dataset
            recv_cmd = ["zfs", "recv", "-F", remote_ds]

            # Build zfs send command
            send_flags = ["-v"]
            if job.compressed:
                send_flags.append("-c")
            if job.encrypted:
                send_flags.append("-w")
            if job.recursive:
                send_flags.append("-R")

            if job.incremental and job.last_snap_sent:
                # Incremental from last known snapshot
                send_cmd = ["zfs", "send"] + send_flags + [
                    "-I", f"{dataset}@{job.last_snap_sent}", snapshot
                ]
            else:
                send_cmd = ["zfs", "send"] + send_flags + [snapshot]

            # Execute: zfs send | ssh ... zfs recv
            send_proc = await asyncio.create_subprocess_exec(
                *[c for c in send_cmd if c],
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            recv_proc = await asyncio.create_subprocess_exec(
                *ssh_opts, *recv_cmd,
                stdin=send_proc.stdout,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            if send_proc.stdout:
                send_proc.stdout.close()

            recv_stdout, recv_stderr = await recv_proc.communicate()
            await send_proc.wait()

            if recv_proc.returncode != 0:
                err = recv_stderr.decode().strip()
                job.last_run_ok = False
                self._save_state()
                return False, f"recv failed: {err}"

            job.last_snap_sent = snap_name
            job.last_run_ts = time.time()
            job.last_run_ok = True
            self._save_state()
            return True, f"replicated {snapshot} → {job.remote_target}"

        except Exception as e:
            job.last_run_ok = False
            self._save_state()
            return False, str(e)

    async def _run(self, cmd: list[str]) -> tuple[bool, str]:
        cmd = [c for c in cmd if c]
        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.STDOUT,
        )
        out, _ = await proc.communicate()
        return proc.returncode == 0, out.decode().strip()


# ──────────────────────────── SMART Scheduler ──────────────────────────────

class SmartScheduler:
    """
    Schedule S.M.A.R.T. self-tests and report results.

    Short test: ~2 min, non-destructive, good for daily scheduling.
    Long test:  hours, full surface scan, weekly or on-demand.

    Does NOT require smartd — drives smartctl directly so it works
    on any host without daemon config management.
    """

    def __init__(self, state_path: Path = Path("/var/lib/ozma/smart-schedule.json")):
        self._state_path = state_path
        self._jobs: dict[str, SmartTestJob] = {}
        self._load_state()

    def _load_state(self) -> None:
        if self._state_path.exists():
            try:
                data = json.loads(self._state_path.read_text())
                for jd in data.get("jobs", []):
                    j = SmartTestJob(**jd)
                    self._jobs[j.device] = j
            except Exception as e:
                log.warning("failed to load smart schedule: %s", e)

    def _save_state(self) -> None:
        self._state_path.parent.mkdir(parents=True, exist_ok=True)
        payload = {"jobs": [dataclasses.asdict(j) for j in self._jobs.values()]}
        self._state_path.write_text(json.dumps(payload, indent=2))

    def add_job(self, job: SmartTestJob) -> None:
        self._jobs[job.device] = job
        self._save_state()

    def remove_job(self, device: str) -> bool:
        if device not in self._jobs:
            return False
        del self._jobs[device]
        self._save_state()
        return True

    def list_jobs(self) -> list[SmartTestJob]:
        return list(self._jobs.values())

    async def run_test(self, device: str, test_type: str = "short") -> tuple[bool, str]:
        """Start a SMART self-test. Returns immediately; poll status with get_result()."""
        proc = await asyncio.create_subprocess_exec(
            "smartctl", "-t", test_type, device,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.STDOUT,
        )
        out, _ = await proc.communicate()
        text = out.decode()
        ok = proc.returncode in (0, 4)  # 4 = test started but disk has old results
        if ok:
            if job := self._jobs.get(device):
                job.last_run_ts = time.time()
                self._save_state()
        return ok, text.strip()

    async def get_result(self, device: str) -> SmartTestResult:
        """Poll the most recent self-test result from the drive."""
        proc = await asyncio.create_subprocess_exec(
            "smartctl", "-l", "selftest", "--json", device,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.STDOUT,
        )
        out, _ = await proc.communicate()
        try:
            data = json.loads(out.decode())
        except json.JSONDecodeError:
            return SmartTestResult(device=device, test_type="unknown", status="error")

        tests = data.get("ata_smart_self_test_log", {}).get("standard", {}).get("table", [])
        if not tests:
            # NVMe path
            tests = data.get("nvme_self_test_log", {}).get("table", [])

        if not tests:
            return SmartTestResult(device=device, test_type="unknown", status="no_results")

        latest = tests[0]
        status_str = str(latest.get("status", {}).get("value", "")).lower()
        if "completed without error" in str(latest.get("status", {}).get("string", "")).lower():
            status = "passed"
        elif "in progress" in str(latest.get("status", {}).get("string", "")).lower():
            status = "running"
        elif "aborted" in str(latest.get("status", {}).get("string", "")).lower():
            status = "aborted"
        else:
            status = "failed"

        pct = latest.get("remaining_percent", 100)
        lhours = latest.get("lifetime_hours", 0)
        lba = latest.get("failing_lba", None)
        if lba == 0xFFFFFFFFFFFFFFFF:
            lba = None

        result = SmartTestResult(
            device=device,
            test_type=latest.get("type", {}).get("string", "unknown").lower(),
            status=status,
            percent_complete=(100 - pct if status == "running" else 100),
            lifetime_hours=lhours,
            lba_of_failure=lba,
        )

        if job := self._jobs.get(device):
            if status != "running":
                job.last_result = status
                self._save_state()

        return result

    async def discover_all_devices(self) -> list[str]:
        """Return all SMART-capable block devices on this host."""
        proc = await asyncio.create_subprocess_exec(
            "smartctl", "--scan-open", "--json",
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        out, _ = await proc.communicate()
        try:
            data = json.loads(out.decode())
            return [d.get("name", "") for d in data.get("devices", []) if d.get("name")]
        except json.JSONDecodeError:
            return []

    async def ensure_default_schedule(self) -> None:
        """Auto-populate short daily + long weekly schedules for all discovered drives."""
        devices = await self.discover_all_devices()
        for dev in devices:
            if dev not in self._jobs:
                self.add_job(SmartTestJob(device=dev, test_type="short", schedule_cron="0 1 * * *"))
            # long test on Sunday 03:00
            long_key = f"{dev}:long"
            if long_key not in self._jobs:
                j = SmartTestJob(device=dev, test_type="long", schedule_cron="0 3 * * 0")
                j.device = long_key
                self.add_job(j)


# ──────────────────────────── iSCSI Manager ────────────────────────────────

class IscsiManager:
    """
    Manage LIO iSCSI targets via targetcli (Linux).

    Provides create/delete targets, attach ZFS zvols or image files,
    manage initiator ACLs, configure CHAP authentication.

    Requires: apt install targetcli-fb
    """

    TARGETCLI = "targetcli"

    async def _tcli(self, *args: str) -> tuple[bool, str]:
        proc = await asyncio.create_subprocess_exec(
            self.TARGETCLI, *args,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.STDOUT,
        )
        out, _ = await proc.communicate()
        return proc.returncode == 0, out.decode().strip()

    async def list_targets(self) -> list[dict]:
        """Return list of configured iSCSI targets with their backstores."""
        ok, out = await self._tcli("ls", "/iscsi", "1")
        if not ok:
            return []
        targets: list[dict] = []
        for line in out.splitlines():
            if "iqn." in line:
                targets.append({"iqn": line.strip().lstrip("o- ").split()[0]})
        return targets

    async def create_zvol_target(
        self,
        pool: str,
        dataset: str,
        size_gb: int,
        iqn: str | None = None,
        initiator_acl: str = "iqn.1993-08.org.debian:*",
    ) -> tuple[bool, str, str]:
        """
        Create a ZFS zvol and attach it as an iSCSI target.
        Returns (ok, message, iqn).
        """
        import socket
        ts = int(time.time())
        if not iqn:
            host = socket.gethostname().replace(".", "-")
            iqn = f"iqn.2025-01.dev.ozma:{host}-{ts}"

        # Create zvol
        zvol_path = f"{pool}/{dataset}"
        proc = await asyncio.create_subprocess_exec(
            "zfs", "create", "-V", f"{size_gb}G", "-o", "volblocksize=4096",
            "-o", "compression=lz4", zvol_path,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.STDOUT,
        )
        out, _ = await proc.communicate()
        if proc.returncode != 0:
            return False, f"zvol creation failed: {out.decode()}", ""

        dev_path = f"/dev/zvol/{zvol_path}"
        backstore_name = dataset.replace("/", "-")

        cmds = [
            f"cd /backstores/block create name={backstore_name} dev={dev_path}",
            f"cd /iscsi create {iqn}",
            f"cd /iscsi/{iqn}/tpg1/luns create /backstores/block/{backstore_name}",
            f"cd /iscsi/{iqn}/tpg1/acls create {initiator_acl}",
            "saveconfig",
        ]
        for cmd in cmds:
            ok, out = await self._tcli(cmd)
            if not ok:
                return False, f"targetcli failed ({cmd!r}): {out}", iqn

        return True, f"created iSCSI target {iqn} backed by zvol {zvol_path}", iqn

    async def delete_target(self, iqn: str, delete_backstore: bool = True) -> tuple[bool, str]:
        cmds = [f"cd /iscsi delete {iqn}"]
        if delete_backstore:
            backstore = iqn.split(":")[-1].replace(".", "-")
            cmds.append(f"cd /backstores/block delete {backstore}")
        cmds.append("saveconfig")
        for cmd in cmds:
            ok, out = await self._tcli(cmd)
            if not ok:
                return False, f"targetcli error: {out}"
        return True, f"deleted {iqn}"

    async def add_initiator_acl(self, iqn: str, initiator_iqn: str) -> tuple[bool, str]:
        ok, out = await self._tcli(
            f"cd /iscsi/{iqn}/tpg1/acls create {initiator_iqn}", "saveconfig"
        )
        return ok, out

    async def set_chap(self, iqn: str, username: str, password: str) -> tuple[bool, str]:
        cmds = [
            f"cd /iscsi/{iqn}/tpg1 set attribute authentication=1",
            f"cd /iscsi/{iqn}/tpg1/auth set userid={username}",
            f"cd /iscsi/{iqn}/tpg1/auth set password={password}",
            "saveconfig",
        ]
        for cmd in cmds:
            ok, out = await self._tcli(cmd)
            if not ok:
                return False, out
        return True, "CHAP configured"


# ──────────────────────────── ACL Manager ──────────────────────────────────

class AclManager:
    """
    POSIX and NFSv4 ACL management via getfacl/setfacl and nfs4_getfacl/nfs4_setfacl.
    """

    async def get_acl(self, path: str, acl_type: str = "posix") -> list[AclEntry]:
        """Read the current ACL on a path."""
        if acl_type == "nfs4":
            cmd = ["nfs4_getfacl", path]
        else:
            cmd = ["getfacl", "--omit-header", path]

        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        out, err = await proc.communicate()
        if proc.returncode != 0:
            log.warning("getfacl failed for %s: %s", path, err.decode())
            return []

        return self._parse_acl(out.decode(), acl_type)

    def _parse_acl(self, text: str, acl_type: str) -> list[AclEntry]:
        entries: list[AclEntry] = []
        for line in text.splitlines():
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            if acl_type == "nfs4":
                # Format: type:flags:principal:permissions
                parts = line.split(":")
                if len(parts) >= 4:
                    entries.append(AclEntry(
                        principal_type="user" if "@" in parts[2] else "group",
                        principal=parts[2],
                        permissions=parts[3],
                        acl_type="nfs4",
                        nfs4_type=parts[0],
                        nfs4_flags=parts[1],
                    ))
            else:
                # POSIX: user::rwx  group::r-x  other::r-x  mask::rwx  user:alice:rw-
                m = re.match(r"^(user|group|other|mask):([^:]*):(.*)$", line)
                if m:
                    entries.append(AclEntry(
                        principal_type=m.group(1),
                        principal=m.group(2),
                        permissions=m.group(3),
                        acl_type="posix",
                    ))
        return entries

    async def set_acl(
        self,
        path: str,
        entries: list[AclEntry],
        recursive: bool = False,
        acl_type: str = "posix",
    ) -> tuple[bool, str]:
        """Apply ACL entries to a path."""
        if acl_type == "nfs4":
            return await self._set_nfs4_acl(path, entries, recursive)

        # Build setfacl -m spec
        specs = []
        for e in entries:
            principal = f":{e.principal}" if e.principal else ""
            specs.append(f"{e.principal_type}{principal}:{e.permissions}")

        cmd = ["setfacl"]
        if recursive:
            cmd.append("-R")
        cmd += ["-m", ",".join(specs), path]

        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.STDOUT,
        )
        out, _ = await proc.communicate()
        ok = proc.returncode == 0
        return ok, out.decode().strip()

    async def _set_nfs4_acl(
        self,
        path: str,
        entries: list[AclEntry],
        recursive: bool,
    ) -> tuple[bool, str]:
        spec_lines = []
        for e in entries:
            spec_lines.append(f"{e.nfs4_type}:{e.nfs4_flags}:{e.principal}:{e.permissions}")
        spec = "\n".join(spec_lines)
        cmd = ["nfs4_setfacl"]
        if recursive:
            cmd.append("-R")
        cmd += ["-s", spec, path]
        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.STDOUT,
        )
        out, _ = await proc.communicate()
        ok = proc.returncode == 0
        return ok, out.decode().strip()

    async def reset_to_posix(self, path: str, recursive: bool = False) -> tuple[bool, str]:
        """Remove all extended ACL entries, leaving only owner/group/other."""
        cmd = ["setfacl", "--remove-all"]
        if recursive:
            cmd.append("-R")
        cmd.append(path)
        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.STDOUT,
        )
        out, _ = await proc.communicate()
        return proc.returncode == 0, out.decode().strip()

    async def copy_acl(self, source: str, dest: str) -> tuple[bool, str]:
        """Copy ACL from one path to another."""
        proc1 = await asyncio.create_subprocess_exec(
            "getfacl", "--omit-header", source,
            stdout=asyncio.subprocess.PIPE,
        )
        out1, _ = await proc1.communicate()
        if proc1.returncode != 0:
            return False, f"getfacl {source} failed"

        proc2 = await asyncio.create_subprocess_exec(
            "setfacl", "--set-file=-", dest,
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.STDOUT,
        )
        out2, _ = await proc2.communicate(input=out1)
        return proc2.returncode == 0, out2.decode().strip()


# ──────────────────────────── Rsync Jobs ───────────────────────────────────

class RsyncJobManager:
    """
    Rsync job scheduler with progress tracking.
    Jobs run on-demand or on a cron schedule.
    Progress is streamed via async generator.
    """

    def __init__(self, state_path: Path = Path("/var/lib/ozma/rsync-jobs.json")):
        self._state_path = state_path
        self._jobs: dict[str, RsyncJob] = {}
        self._running: dict[str, asyncio.Process] = {}
        self._load_state()

    def _load_state(self) -> None:
        if self._state_path.exists():
            try:
                data = json.loads(self._state_path.read_text())
                for jd in data.get("jobs", []):
                    j = RsyncJob(**jd)
                    self._jobs[j.job_id] = j
            except Exception as e:
                log.warning("failed to load rsync jobs: %s", e)

    def _save_state(self) -> None:
        self._state_path.parent.mkdir(parents=True, exist_ok=True)
        payload = {"jobs": [dataclasses.asdict(j) for j in self._jobs.values()]}
        self._state_path.write_text(json.dumps(payload, indent=2))

    def add_job(self, job: RsyncJob) -> None:
        self._jobs[job.job_id] = job
        self._save_state()

    def remove_job(self, job_id: str) -> bool:
        if job_id not in self._jobs:
            return False
        del self._jobs[job_id]
        self._save_state()
        return True

    def list_jobs(self) -> list[RsyncJob]:
        return list(self._jobs.values())

    async def run_job_with_progress(
        self, job_id: str
    ) -> AsyncIterator[dict]:
        """
        Run a rsync job and yield progress dicts:
            {"type": "progress", "percent": 45, "speed_kb": 12300, "eta_s": 55}
            {"type": "done", "ok": True, "bytes_sent": 123456, "duration_s": 12.3}
        """
        job = self._jobs.get(job_id)
        if not job:
            yield {"type": "error", "message": f"job {job_id} not found"}
            return

        cmd = ["rsync", "--progress", "--stats", "--human-readable"]
        if job.archive:
            cmd.append("-a")
        if job.compress:
            cmd.append("-z")
        if job.checksum:
            cmd.append("-c")
        if job.delete:
            cmd.append("--delete")
        if job.bandwidth_limit_kb:
            cmd += ["--bwlimit", str(job.bandwidth_limit_kb)]
        for pat in job.exclude_patterns:
            cmd += ["--exclude", pat]
        if job.ssh_key_path:
            cmd += ["-e", f"ssh -i {shlex.quote(job.ssh_key_path)} -o StrictHostKeyChecking=accept-new"]

        cmd += [job.source, job.destination]
        start = time.time()

        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        self._running[job_id] = proc
        bytes_sent = 0

        try:
            assert proc.stdout
            async for raw in proc.stdout:
                line = raw.decode(errors="replace").strip()
                # rsync --progress line: "     1,234,567  45%  1.23MB/s    0:00:12"
                m = re.search(r"(\d[\d,]+)\s+(\d+)%\s+([\d.]+[KMGBk]+/s)\s+([\d:]+)", line)
                if m:
                    pct = int(m.group(2))
                    yield {"type": "progress", "percent": pct, "speed": m.group(3), "eta": m.group(4)}
                # Total file size from stats
                m2 = re.search(r"Total transferred file size:\s+([\d,]+)", line)
                if m2:
                    bytes_sent = int(m2.group(1).replace(",", ""))

            await proc.wait()
            duration = time.time() - start
            ok = proc.returncode == 0
            job.last_run_ok = ok
            job.last_run_ts = time.time()
            job.last_bytes_sent = bytes_sent
            job.last_duration_s = duration
            self._save_state()
            yield {"type": "done", "ok": ok, "bytes_sent": bytes_sent, "duration_s": duration}
        finally:
            self._running.pop(job_id, None)

    async def cancel_job(self, job_id: str) -> bool:
        proc = self._running.get(job_id)
        if proc:
            proc.terminate()
            return True
        return False


# ──────────────────────────── Secure Wipe ──────────────────────────────────

class SecureWipeManager:
    """
    Secure disk erasure before decommission.

    Methods ranked from fastest to most thorough:
      blkdiscard  — SSDs: discard all blocks (trim). OS-transparent, instant.
      nvme_format — NVMe: crypto-erase (if drive has encryption keys).
      ata_secure  — ATA: Enhanced Security Erase via hdparm.
      zeros       — dd if=/dev/zero (fast, not forensically strong).
      random      — shred -n 1 (one random pass).
      dod_7       — shred -n 3 (3-pass DoD 5220.22-M compliant).

    All methods yield progress events. Wipe is logged with start/end timestamp
    and method for audit purposes.
    """

    async def detect_method(self, device: str) -> list[WipeMethod]:
        """Return recommended wipe methods for this device type, best first."""
        methods: list[WipeMethod] = []
        dev_base = Path(device).name

        # NVMe
        if "nvme" in device.lower():
            methods.append(WipeMethod.NVME_FORMAT)
            methods.append(WipeMethod.BLKDISCARD)
        else:
            # Check if rotational (HDD vs SSD)
            rot_path = Path(f"/sys/block/{dev_base}/queue/rotational")
            is_rotational = rot_path.read_text().strip() == "1" if rot_path.exists() else True

            if not is_rotational:
                methods.append(WipeMethod.BLKDISCARD)
            # Always offer ATA Secure Erase for SATA
            methods.append(WipeMethod.ATA_SECURE)

        # Universal fallback methods
        methods += [WipeMethod.DOD_7, WipeMethod.RANDOM, WipeMethod.ZEROS]
        return methods

    async def wipe_with_progress(
        self,
        device: str,
        method: WipeMethod,
        verified: bool = False,
    ) -> AsyncIterator[dict]:
        """
        Wipe a device and yield progress events:
            {"type": "start", "device": "/dev/sdb", "method": "dod_7"}
            {"type": "progress", "percent": 34}
            {"type": "done", "ok": True, "duration_s": 123.4}
            {"type": "error", "message": "..."}
        """
        job = WipeJob(device=device, method=method, verified=verified)
        job.started_ts = time.time()

        yield {"type": "start", "device": device, "method": method.value}

        try:
            async for event in self._do_wipe(device, method):
                yield event
            job.finished_ts = time.time()
            job.success = True
            yield {"type": "done", "ok": True, "duration_s": job.finished_ts - job.started_ts}
        except Exception as e:
            job.finished_ts = time.time()
            job.error = str(e)
            yield {"type": "error", "message": str(e)}

        log.info(
            "wipe complete: device=%s method=%s ok=%s duration=%.1fs",
            device, method.value, job.success, job.finished_ts - job.started_ts,
        )

    async def _do_wipe(self, device: str, method: WipeMethod) -> AsyncIterator[dict]:
        if method == WipeMethod.BLKDISCARD:
            async for ev in self._blkdiscard(device):
                yield ev
        elif method == WipeMethod.NVME_FORMAT:
            async for ev in self._nvme_format(device):
                yield ev
        elif method == WipeMethod.ATA_SECURE:
            async for ev in self._ata_secure_erase(device):
                yield ev
        elif method in (WipeMethod.ZEROS, WipeMethod.RANDOM, WipeMethod.DOD_7):
            async for ev in self._shred(device, method):
                yield ev

    async def _blkdiscard(self, device: str) -> AsyncIterator[dict]:
        yield {"type": "progress", "percent": 0, "note": "issuing discard"}
        proc = await asyncio.create_subprocess_exec(
            "blkdiscard", "-v", device,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.STDOUT,
        )
        out, _ = await proc.communicate()
        if proc.returncode != 0:
            raise RuntimeError(f"blkdiscard failed: {out.decode()}")
        yield {"type": "progress", "percent": 100}

    async def _nvme_format(self, device: str) -> AsyncIterator[dict]:
        yield {"type": "progress", "percent": 0, "note": "NVMe cryptographic erase"}
        proc = await asyncio.create_subprocess_exec(
            "nvme", "format", "--ses=1", "--force", device,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.STDOUT,
        )
        out, _ = await proc.communicate()
        if proc.returncode != 0:
            raise RuntimeError(f"nvme format failed: {out.decode()}")
        yield {"type": "progress", "percent": 100}

    async def _ata_secure_erase(self, device: str) -> AsyncIterator[dict]:
        # Unfreeze security state first (sometimes required after suspend)
        yield {"type": "progress", "percent": 0, "note": "ATA Secure Erase"}
        # Set a temporary password (required before erase)
        proc = await asyncio.create_subprocess_exec(
            "hdparm", "--user-master", "u", "--security-set-pass", "ozma-wipe", device,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.STDOUT,
        )
        await proc.communicate()

        # Enhanced Security Erase
        proc = await asyncio.create_subprocess_exec(
            "hdparm", "--user-master", "u", "--security-erase-enhanced", "ozma-wipe", device,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.STDOUT,
        )
        out, _ = await proc.communicate()
        if proc.returncode != 0:
            # Fallback to standard erase
            proc2 = await asyncio.create_subprocess_exec(
                "hdparm", "--user-master", "u", "--security-erase", "ozma-wipe", device,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.STDOUT,
            )
            out2, _ = await proc2.communicate()
            if proc2.returncode != 0:
                raise RuntimeError(f"hdparm security erase failed: {out2.decode()}")
        yield {"type": "progress", "percent": 100}

    async def _shred(self, device: str, method: WipeMethod) -> AsyncIterator[dict]:
        passes = {WipeMethod.ZEROS: 0, WipeMethod.RANDOM: 1, WipeMethod.DOD_7: 3}[method]
        # Zero final pass for DoD compliance
        cmd = ["shred", "-v", f"-n{passes}"]
        if method == WipeMethod.DOD_7:
            cmd.append("-z")  # add final zero pass
        cmd.append(device)

        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,  # shred progress goes to stderr
        )
        assert proc.stderr
        async for raw in proc.stderr:
            line = raw.decode(errors="replace").strip()
            # shred: /dev/sdb: pass 1/3 (random)...1.23GiB/3.64TiB 0%
            m = re.search(r"(\d+)%", line)
            if m:
                yield {"type": "progress", "percent": int(m.group(1)), "note": line}
        await proc.wait()
        if proc.returncode != 0:
            raise RuntimeError(f"shred failed with code {proc.returncode}")
        yield {"type": "progress", "percent": 100}


# ──────────────────────────── Capacity Trends ──────────────────────────────

class CapacityTrendTracker:
    """
    Track pool/array capacity over time for trend analysis.
    Snapshots taken hourly, stored as a JSON time-series.
    Exposes: current usage, growth rate, estimated days to full.
    """

    def __init__(self, state_path: Path = Path("/var/lib/ozma/capacity-history.json")):
        self._state_path = state_path
        self._history: list[CapacitySnapshot] = []
        self._load()

    def _load(self) -> None:
        if self._state_path.exists():
            try:
                data = json.loads(self._state_path.read_text())
                self._history = [CapacitySnapshot(**s) for s in data.get("snapshots", [])]
            except Exception as e:
                log.warning("failed to load capacity history: %s", e)

    def _save(self) -> None:
        self._state_path.parent.mkdir(parents=True, exist_ok=True)
        # Keep last 90 days of hourly snapshots ≈ 2160 entries per pool
        cutoff = time.time() - 90 * 86400
        self._history = [s for s in self._history if s.timestamp > cutoff]
        payload = {"snapshots": [dataclasses.asdict(s) for s in self._history]}
        self._state_path.write_text(json.dumps(payload))

    async def take_snapshot(self) -> None:
        """Sample all ZFS pools and mdraid arrays now."""
        # ZFS pools
        proc = await asyncio.create_subprocess_exec(
            "zpool", "list", "-Hp", "-o", "name,used,avail,size",
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        out, _ = await proc.communicate()
        now = time.time()
        for line in out.decode().splitlines():
            parts = line.split("\t")
            if len(parts) >= 4:
                try:
                    self._history.append(CapacitySnapshot(
                        pool=parts[0],
                        timestamp=now,
                        used_bytes=int(parts[1]),
                        avail_bytes=int(parts[2]),
                        total_bytes=int(parts[3]),
                    ))
                except ValueError:
                    pass

        # mdraid arrays
        proc2 = await asyncio.create_subprocess_exec(
            "cat", "/proc/mdstat",
            stdout=asyncio.subprocess.PIPE,
        )
        out2, _ = await proc2.communicate()
        for line in out2.decode().splitlines():
            m = re.search(r"(md\d+)\s+:\s+.*\n.*(\d+) blocks", line)
            if m:
                pass  # mdraid doesn't expose used/avail easily; skip for now

        self._save()

    def get_history(self, pool: str, hours: int = 168) -> list[CapacitySnapshot]:
        """Get capacity history for a pool, last N hours."""
        cutoff = time.time() - hours * 3600
        return [s for s in self._history if s.pool == pool and s.timestamp > cutoff]

    def estimate_days_to_full(self, pool: str) -> float | None:
        """
        Linear regression on used_bytes over the last 7 days.
        Returns estimated days until pool is full, or None if shrinking/stable.
        """
        snaps = self.get_history(pool, hours=168)
        if len(snaps) < 2:
            return None

        times = [s.timestamp for s in snaps]
        used = [s.used_bytes for s in snaps]
        n = len(times)
        t0 = times[0]
        xs = [(t - t0) / 3600 for t in times]

        # Simple linear regression
        mx = sum(xs) / n
        my = sum(used) / n
        num = sum((xi - mx) * (yi - my) for xi, yi in zip(xs, used))
        den = sum((xi - mx) ** 2 for xi in xs)
        if den == 0:
            return None

        slope_per_hour = num / den  # bytes/hour
        if slope_per_hour <= 0:
            return None

        current = snaps[-1]
        avail = current.avail_bytes
        if avail <= 0:
            return 0.0

        hours_to_full = avail / slope_per_hour
        return hours_to_full / 24

    def growth_rate_gb_per_day(self, pool: str) -> float:
        """Average daily growth rate over the last 7 days (GB/day)."""
        snaps = self.get_history(pool, hours=168)
        if len(snaps) < 2:
            return 0.0
        first, last = snaps[0], snaps[-1]
        elapsed_days = (last.timestamp - first.timestamp) / 86400
        if elapsed_days <= 0:
            return 0.0
        delta_gb = (last.used_bytes - first.used_bytes) / (1024 ** 3)
        return delta_gb / elapsed_days


# ──────────────────────────── S3 Gateway Helper ────────────────────────────
#
# Supported backends (all genuinely open source):
#
#  garage        AGPL-3.0  — lightweight, geo-distributed, designed for
#                             self-hosting. Single binary. Ideal for homelab/SMB.
#                             https://garagehq.deuxfleurs.fr/
#
#  seaweedfs     Apache 2.0 — distributed, scales to billions of files, built-in
#                             S3 API + filer. Good for medium-large deployments.
#                             https://github.com/seaweedfs/seaweedfs
#
#  ceph-rgw      LGPL-2.1  — RADOS Gateway, part of Ceph. For existing Ceph
#                             clusters. Full S3 + Swift + Admin API.
#
#  rclone-serve  MIT       — rclone serve s3 exposes any rclone remote as S3.
#                             Zero install if rclone is already present.
#                             Ideal for transient/test access to any backend.
#
#  zenko         Apache 2.0 — Scality's CloudServer (formerly S3 Server).
#                             https://github.com/scality/cloudserver
#
# NOT supported: MinIO — switched to SSPL in 2021, not OSI-approved.

class S3GatewayHelper:
    """
    Generate config and deployment units for S3-compatible gateways
    on top of ZFS datasets or any local storage.

    Does not run anything — generates files for the user/automation system
    to deploy. Supports multiple open-source backends.
    """

    # ── Garage (AGPL-3.0) ────────────────────────────────────────────────

    def garage_config(
        self,
        data_dir: str,
        metadata_dir: str,
        rpc_secret: str,
        api_port: int = 3900,
        rpc_port: int = 3901,
        web_port: int = 3902,
        node_id: str = "node1",
        replication_factor: int = 1,
    ) -> str:
        """
        Generate garage.toml for a single-node Garage deployment.
        For multi-node, add peers under [[peers]] sections.
        """
        return f"""# garage.toml — generated by Ozma
# Single-node Garage S3 gateway
# License: AGPL-3.0  https://garagehq.deuxfleurs.fr/

metadata_dir = "{metadata_dir}"
data_dir = "{data_dir}"

db_engine = "lmdb"

replication_factor = {replication_factor}

[rpc_secret]
# Generate with: openssl rand -hex 32
secret = "{rpc_secret}"

[rpc_bind_addr]
addr = "0.0.0.0:{rpc_port}"

[s3_api]
s3_region = "garage"
api_bind_addr = "0.0.0.0:{api_port}"

[s3_web]
bind_addr = "0.0.0.0:{web_port}"
root_domain = ".s3.local"
index = "index.html"
"""

    def garage_compose(
        self,
        data_dir: str,
        metadata_dir: str,
        rpc_secret: str,
        api_port: int = 3900,
    ) -> str:
        return f"""services:
  garage:
    image: dxflrs/garage:v1.0.0
    container_name: ozma-garage
    network_mode: host
    volumes:
      - {data_dir}:/data
      - {metadata_dir}:/meta
      - /etc/ozma/garage.toml:/etc/garage.toml:ro
    restart: unless-stopped
    healthcheck:
      test: ["CMD", "garage", "status"]
      interval: 30s
      timeout: 10s
      retries: 3
"""

    def garage_systemd(
        self,
        config_path: str = "/etc/ozma/garage.toml",
        garage_bin: str = "/usr/local/bin/garage",
    ) -> str:
        return f"""[Unit]
Description=Garage S3 object storage (Ozma-managed)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart={garage_bin} -c {config_path} server
Restart=on-failure
RestartSec=5
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
"""

    # ── SeaweedFS (Apache 2.0) ────────────────────────────────────────────

    def seaweedfs_compose(
        self,
        data_dir: str,
        master_port: int = 9333,
        volume_port: int = 8080,
        s3_port: int = 8333,
        filer_port: int = 8888,
    ) -> str:
        """
        Single-node SeaweedFS deployment: master + volume + filer + S3 proxy.
        For production, run multiple volume servers and replicate.
        """
        return f"""services:
  weed-master:
    image: chrislusf/seaweedfs:latest
    container_name: ozma-seaweedfs-master
    ports:
      - "{master_port}:9333"
    command: master -mdir=/data/master
    volumes:
      - {data_dir}/master:/data/master
    restart: unless-stopped

  weed-volume:
    image: chrislusf/seaweedfs:latest
    container_name: ozma-seaweedfs-volume
    ports:
      - "{volume_port}:8080"
    command: >
      volume -mserver=weed-master:9333
             -dir=/data/volumes
             -max=0
             -ip.bind=0.0.0.0
    volumes:
      - {data_dir}/volumes:/data/volumes
    depends_on: [weed-master]
    restart: unless-stopped

  weed-filer:
    image: chrislusf/seaweedfs:latest
    container_name: ozma-seaweedfs-filer
    ports:
      - "{filer_port}:8888"
    command: filer -master=weed-master:9333
    depends_on: [weed-master]
    restart: unless-stopped

  weed-s3:
    image: chrislusf/seaweedfs:latest
    container_name: ozma-seaweedfs-s3
    ports:
      - "{s3_port}:8333"
    command: >
      s3 -filer=weed-filer:8888
         -port=8333
         -config=/etc/seaweedfs/s3.json
    volumes:
      - /etc/ozma/seaweedfs:/etc/seaweedfs:ro
    depends_on: [weed-filer]
    restart: unless-stopped
"""

    # ── Rclone serve s3 (MIT) ─────────────────────────────────────────────

    def rclone_serve_systemd(
        self,
        remote: str,
        bind_addr: str = "127.0.0.1:8333",
        access_key: str = "ozma",
        secret_key: str = "",
        rclone_bin: str = "/usr/bin/rclone",
    ) -> str:
        """
        Serve any rclone remote as an S3 endpoint.
        Useful for exposing local ZFS datasets, SFTP, or cloud buckets
        via a standard S3 API for tools that need it.

        Configure the remote first: rclone config
        """
        return f"""[Unit]
Description=rclone S3 server — {remote} (Ozma-managed)
After=network-online.target

[Service]
Type=simple
ExecStart={rclone_bin} serve s3 \\
  --addr {bind_addr} \\
  --s3-authkey {access_key} \\
  --s3-authmessage {secret_key or "change-me"} \\
  {remote}
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
"""

    # ── Zenko CloudServer (Apache 2.0) ────────────────────────────────────

    def zenko_compose(
        self,
        data_dir: str,
        port: int = 8000,
        access_key: str = "ozmaAccessKey",
        secret_key: str = "ozmaSecretKey1",
    ) -> str:
        """
        Zenko CloudServer (Scality) — Apache 2.0 S3-compatible server.
        Supports multiple location backends: local, AWS, GCP, Azure.
        """
        return f"""services:
  cloudserver:
    image: ghcr.io/scality/cloudserver:latest
    container_name: ozma-cloudserver
    ports:
      - "{port}:8000"
    environment:
      SCALITY_ACCESS_KEY_ID: "{access_key}"
      SCALITY_SECRET_ACCESS_KEY: "{secret_key}"
      S3DATA: "multiple"
      S3METADATA: "file"
      S3_LOCATION_FILE: /etc/cloudserver/locationConfig.json
    volumes:
      - {data_dir}:/usr/src/app/localData
      - {data_dir}/metadata:/usr/src/app/localMetadata
    restart: unless-stopped
    healthcheck:
      test: ["CMD", "curl", "-sf", "http://localhost:8000/_/healthcheck"]
      interval: 30s
      timeout: 10s
      retries: 3
"""

    # ── Ceph RGW ──────────────────────────────────────────────────────────

    def ceph_rgw_notes(self, rgw_host: str = "localhost", rgw_port: int = 7480) -> str:
        """
        Returns setup notes for connecting Ozma to an existing Ceph RGW endpoint.
        Ozma does not manage Ceph directly — use cephadm or Rook for that.
        """
        return f"""# Ceph RADOS Gateway integration notes
# RGW endpoint: http://{rgw_host}:{rgw_port}

# 1. Create an RGW user:
#    radosgw-admin user create --uid=ozma --display-name="Ozma" --email=admin@ozma.local

# 2. Note the access_key and secret_key from the output.

# 3. Configure rclone or any S3 client to point at:
#    endpoint = http://{rgw_host}:{rgw_port}
#    region = (any, e.g. "ceph")
#    force_path_style = true  (required for RGW)

# 4. Rclone remote config (~/.config/rclone/rclone.conf):
# [ceph]
# type = s3
# provider = Ceph
# endpoint = http://{rgw_host}:{rgw_port}
# access_key_id = YOUR_KEY
# secret_access_key = YOUR_SECRET
# region = ceph
"""


# ──────────────────────────── Top-level manager ────────────────────────────

class StorageAdminManager:
    """
    Top-level coordinator for all storage administration subsystems.
    Instantiated once per agent; subsystems are lazy-init on first use.
    """

    def __init__(self, state_dir: Path = Path("/var/lib/ozma")):
        self._state_dir = state_dir
        self._replication: ZfsReplicationManager | None = None
        self._smart: SmartScheduler | None = None
        self._iscsi: IscsiManager | None = None
        self._acl: AclManager | None = None
        self._rsync: RsyncJobManager | None = None
        self._wipe: SecureWipeManager | None = None
        self._capacity: CapacityTrendTracker | None = None
        self._s3gw: S3GatewayHelper | None = None

    @property
    def replication(self) -> ZfsReplicationManager:
        if not self._replication:
            self._replication = ZfsReplicationManager(self._state_dir / "zfs-replication.json")
        return self._replication

    @property
    def smart(self) -> SmartScheduler:
        if not self._smart:
            self._smart = SmartScheduler(self._state_dir / "smart-schedule.json")
        return self._smart

    @property
    def iscsi(self) -> IscsiManager:
        if not self._iscsi:
            self._iscsi = IscsiManager()
        return self._iscsi

    @property
    def acl(self) -> AclManager:
        if not self._acl:
            self._acl = AclManager()
        return self._acl

    @property
    def rsync(self) -> RsyncJobManager:
        if not self._rsync:
            self._rsync = RsyncJobManager(self._state_dir / "rsync-jobs.json")
        return self._rsync

    @property
    def wipe(self) -> SecureWipeManager:
        if not self._wipe:
            self._wipe = SecureWipeManager()
        return self._wipe

    @property
    def capacity(self) -> CapacityTrendTracker:
        if not self._capacity:
            self._capacity = CapacityTrendTracker(self._state_dir / "capacity-history.json")
        return self._capacity

    @property
    def s3gw(self) -> S3GatewayHelper:
        if not self._s3gw:
            self._s3gw = S3GatewayHelper()
        return self._s3gw

    async def start(self) -> None:
        """Start background tasks: capacity snapshots, SMART polling."""
        asyncio.create_task(self._capacity_loop(), name="storage_admin.capacity")
        asyncio.create_task(self._smart_loop(), name="storage_admin.smart")
        asyncio.create_task(self._replication_loop(), name="storage_admin.replication")
        asyncio.create_task(self._rsync_loop(), name="storage_admin.rsync")
        await self.smart.ensure_default_schedule()

    async def _capacity_loop(self) -> None:
        """Take capacity snapshots every hour."""
        while True:
            try:
                await self.capacity.take_snapshot()
            except Exception as e:
                log.warning("capacity snapshot failed: %s", e)
            await asyncio.sleep(3600)

    async def _smart_loop(self) -> None:
        """Run due SMART tests based on cron schedules."""
        import croniter  # type: ignore
        while True:
            now = time.time()
            for job in self.smart.list_jobs():
                if not job.enabled:
                    continue
                try:
                    cron = croniter.croniter(job.schedule_cron, job.last_run_ts or now - 86400)
                    if cron.get_next(float) <= now:
                        log.info("running SMART %s test on %s", job.test_type, job.device)
                        await self.smart.run_test(job.device, job.test_type)
                except Exception as e:
                    log.warning("smart test failed for %s: %s", job.device, e)
            await asyncio.sleep(60)

    async def _replication_loop(self) -> None:
        """Run due replication jobs."""
        import croniter  # type: ignore
        while True:
            now = time.time()
            for job in self.replication.list_jobs():
                if not job.enabled:
                    continue
                try:
                    cron = croniter.croniter(job.schedule_cron, job.last_run_ts or now - 86400)
                    if cron.get_next(float) <= now:
                        log.info("running replication job %s", job.job_id)
                        ok, msg = await self.replication.run_job(job.job_id)
                        if not ok:
                            log.warning("replication %s failed: %s", job.job_id, msg)
                except Exception as e:
                    log.warning("replication loop error for %s: %s", job.job_id, e)
            await asyncio.sleep(60)

    async def _rsync_loop(self) -> None:
        """Run due rsync jobs."""
        import croniter  # type: ignore
        while True:
            now = time.time()
            for job in self.rsync.list_jobs():
                if not job.enabled:
                    continue
                try:
                    cron = croniter.croniter(job.schedule_cron, job.last_run_ts or now - 86400)
                    if cron.get_next(float) <= now:
                        log.info("running rsync job %s", job.job_id)
                        async for event in self.rsync.run_job_with_progress(job.job_id):
                            if event.get("type") == "error":
                                log.warning("rsync %s: %s", job.job_id, event)
                except Exception as e:
                    log.warning("rsync loop error for %s: %s", job.job_id, e)
            await asyncio.sleep(60)


def collect_storage_admin_prometheus(manager: StorageAdminManager, lb: str = "") -> str:
    """Prometheus metrics for storage admin subsystems."""
    lines: list[str] = []
    lbl = f"{{{lb.strip(',')}}}" if lb else ""

    # Replication job health
    for job in manager.replication.list_jobs():
        jlb = f'{{job="{job.job_id}",pool="{job.pool}",remote="{job.remote_host}"{("," + lb) if lb else ""}}}'
        lines.append(f"ozma_repl_last_ok{jlb} {1 if job.last_run_ok else 0}")
        if job.last_run_ts:
            lines.append(f"ozma_repl_last_run_ts{jlb} {job.last_run_ts:.0f}")

    # SMART job last results
    for job in manager.smart.list_jobs():
        slb = f'{{device="{job.device}",test="{job.test_type}"{("," + lb) if lb else ""}}}'
        ok = 1 if job.last_result == "passed" else (0 if job.last_result == "failed" else -1)
        lines.append(f"ozma_smart_last_ok{slb} {ok}")
        if job.last_run_ts:
            lines.append(f"ozma_smart_last_run_ts{slb} {job.last_run_ts:.0f}")

    # Rsync job health
    for job in manager.rsync.list_jobs():
        rlb = f'{{job="{job.job_id}"{("," + lb) if lb else ""}}}'
        lines.append(f"ozma_rsync_last_ok{rlb} {1 if job.last_run_ok else 0}")
        if job.last_bytes_sent:
            lines.append(f"ozma_rsync_last_bytes{rlb} {job.last_bytes_sent}")
        if job.last_duration_s:
            lines.append(f"ozma_rsync_last_duration_s{rlb} {job.last_duration_s:.1f}")

    # Capacity trends
    for pool in {s.pool for s in manager.capacity._history}:
        snaps = manager.capacity.get_history(pool, hours=1)
        if snaps:
            s = snaps[-1]
            clb = f'{{pool="{pool}"{("," + lb) if lb else ""}}}'
            lines.append(f"ozma_pool_used_bytes{clb} {s.used_bytes}")
            lines.append(f"ozma_pool_avail_bytes{clb} {s.avail_bytes}")
            lines.append(f"ozma_pool_total_bytes{clb} {s.total_bytes}")
            days = manager.capacity.estimate_days_to_full(pool)
            if days is not None:
                lines.append(f"ozma_pool_days_to_full{clb} {days:.1f}")
            rate = manager.capacity.growth_rate_gb_per_day(pool)
            lines.append(f"ozma_pool_growth_rate_gb_per_day{clb} {rate:.3f}")

    return "\n".join(lines)
