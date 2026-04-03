# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Ozma node backup — Restic-based, cross-platform, zero-touch.

Four backup modes
─────────────────
  files       — home directories and user data.  Fast, safe default.
  smart       — same as files but excludes package-manager-installed apps
                (they can be reinstalled).  Uses app inventory to be precise
                rather than pattern-matching.
  disk_image  — full disk/partition image via partclone → Restic block store.
                Disaster recovery: node presents bootable USB via virtual_media.
  advanced    — custom include/exclude lists, full control.

Destinations
────────────
  local       — path on the agent machine's filesystem
  s3          — any S3-compatible endpoint (Backblaze B2, Wasabi, MinIO, AWS)
  sftp        — SSH/SFTP to a NAS or remote host
  rest        — Restic REST server protocol
  connect_s3  — Ozma Connect S3 with zero-knowledge encryption
                (Restic password = key_store.derive_subkey("backup").hex();
                 Connect stores only ciphertext)

Zero-knowledge encryption
─────────────────────────
For connect_s3 destinations (and any destination when encrypt=True), the
Restic repository password is derived from the controller master key:

    password = key_store.derive_subkey("backup").hex()   # 64-char hex string

Without the master key, even Ozma Labs cannot read backup data.

Adaptive scheduling
───────────────────
When schedule="adaptive" (default), the backup runs automatically when:
  - Last backup was > 24 hours ago
  - Machine has been idle for > 5 minutes
  - Not in a meeting (checked via meeting_detect if available)
  - Battery > 20% or plugged in (or desktop — always OK)
  - Network bandwidth allows it (dynamic --limit-upload)

The scheduler checks conditions every 15 minutes.

Business backup
───────────────
append_only=True → `restic backup --no-lock` + Connect S3 with Object Lock
(Compliance mode).  The Restic repo can only be pruned by the secondary
administrative domain (server-side in Connect), not by the client.
This satisfies Essential Eight ML3, SOC 2, ISO 27001 A.12.3, HIPAA.

Platform support
────────────────
  Linux    — full support (all modes)
  macOS    — files/smart/advanced; disk_image limited (Apple Secure Boot)
  Windows  — files/smart/advanced with VSS (--use-fs-snapshot)
  FreeBSD  — ZFS-native (zfs send for disk_image mode)
  OpenBSD  — FFS dump/restore for disk_image

Usage (in-process)
──────────────────
    mgr = BackupManager(data_dir=Path("~/.ozma/backup"))
    await mgr.start()
    status = await mgr.run_backup()
    await mgr.stop()
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import platform
import shutil
import subprocess
import sys
import time
import uuid
from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.agent.backup")

# ---------------------------------------------------------------------------
# Enumerations
# ---------------------------------------------------------------------------

class BackupMode(str, Enum):
    FILES      = "files"       # home dir + user data
    SMART      = "smart"       # files minus package-managed apps
    DISK_IMAGE = "disk_image"  # full disk via partclone/dd
    ADVANCED   = "advanced"    # custom include/exclude lists


class BackupDestination(str, Enum):
    LOCAL      = "local"       # local path
    S3         = "s3"          # S3-compatible
    SFTP       = "sftp"        # SSH/SFTP
    REST       = "rest"        # Restic REST server
    CONNECT_S3 = "connect_s3"  # Ozma Connect ZK


class BackupHealth(str, Enum):
    GREEN  = "green"   # last success < 3 days
    YELLOW = "yellow"  # 3-7 days or no backup yet
    ORANGE = "orange"  # 7-14 days or 2+ consecutive failures
    RED    = "red"     # >14 days or 3+ consecutive failures
    UNCONFIGURED = "unconfigured"


# ---------------------------------------------------------------------------
# Data models
# ---------------------------------------------------------------------------

@dataclass
class RetentionPolicy:
    daily:   int = 7
    weekly:  int = 4
    monthly: int = 12
    yearly:  int = 0

    def to_forget_args(self) -> list[str]:
        args = []
        if self.daily:   args += ["--keep-daily",   str(self.daily)]
        if self.weekly:  args += ["--keep-weekly",  str(self.weekly)]
        if self.monthly: args += ["--keep-monthly", str(self.monthly)]
        if self.yearly:  args += ["--keep-yearly",  str(self.yearly)]
        return args

    def to_dict(self) -> dict:
        return {"daily": self.daily, "weekly": self.weekly,
                "monthly": self.monthly, "yearly": self.yearly}

    @classmethod
    def from_dict(cls, d: dict) -> "RetentionPolicy":
        return cls(
            daily=d.get("daily", 7), weekly=d.get("weekly", 4),
            monthly=d.get("monthly", 12), yearly=d.get("yearly", 0),
        )


@dataclass
class BackupConfig:
    enabled:          bool               = False
    mode:             BackupMode         = BackupMode.SMART
    destination:      BackupDestination  = BackupDestination.LOCAL
    destination_config: dict             = field(default_factory=dict)
    # destination_config keys:
    #   local:      {"path": "/mnt/backup"}
    #   s3:         {"endpoint": "https://...", "bucket": "...", "prefix": "",
    #                "access_key_id": "...", "secret_access_key": "..."}
    #   sftp:       {"host": "nas.local", "user": "backup", "path": "/backups/",
    #                "identity_file": "~/.ssh/id_ed25519"}
    #   rest:       {"url": "http://restic-server:8000/repo"}
    #   connect_s3: {"endpoint": "...", "bucket": "...", "prefix": "backup/"}

    schedule:         str                = "adaptive"
    retention:        RetentionPolicy    = field(default_factory=RetentionPolicy)
    encrypt:          bool               = True
    append_only:      bool               = False
    verify_weekly:    bool               = True
    db_hooks:         bool               = True
    use_vss:          bool               = True  # Windows VSS
    bandwidth_limit:  int                = 0     # KB/s upload limit; 0 = auto

    # Advanced mode custom lists
    include_paths:    list[str]          = field(default_factory=list)
    extra_excludes:   list[str]          = field(default_factory=list)

    # Health alert dismissal (timestamp until which alerts are suppressed)
    alert_dismissed_until: float         = 0.0

    def to_dict(self) -> dict:
        d = {
            "enabled": self.enabled,
            "mode": self.mode,
            "destination": self.destination,
            "destination_config": self._redact_config(),
            "schedule": self.schedule,
            "retention": self.retention.to_dict(),
            "encrypt": self.encrypt,
            "append_only": self.append_only,
            "verify_weekly": self.verify_weekly,
            "db_hooks": self.db_hooks,
            "use_vss": self.use_vss,
            "bandwidth_limit": self.bandwidth_limit,
            "include_paths": self.include_paths,
            "extra_excludes": self.extra_excludes,
            "alert_dismissed_until": self.alert_dismissed_until,
        }
        return d

    def _redact_config(self) -> dict:
        redacted = dict(self.destination_config)
        for key in ("secret_access_key", "password", "token"):
            if key in redacted:
                redacted[key] = "***"
        return redacted

    @classmethod
    def from_dict(cls, d: dict) -> "BackupConfig":
        return cls(
            enabled=d.get("enabled", False),
            mode=BackupMode(d.get("mode", "smart")),
            destination=BackupDestination(d.get("destination", "local")),
            destination_config=d.get("destination_config", {}),
            schedule=d.get("schedule", "adaptive"),
            retention=RetentionPolicy.from_dict(d.get("retention", {})),
            encrypt=d.get("encrypt", True),
            append_only=d.get("append_only", False),
            verify_weekly=d.get("verify_weekly", True),
            db_hooks=d.get("db_hooks", True),
            use_vss=d.get("use_vss", True),
            bandwidth_limit=d.get("bandwidth_limit", 0),
            include_paths=d.get("include_paths", []),
            extra_excludes=d.get("extra_excludes", []),
            alert_dismissed_until=float(d.get("alert_dismissed_until", 0.0)),
        )


@dataclass
class BackupStatus:
    enabled:             bool               = False
    running:             bool               = False
    progress:            float | None       = None  # 0.0–1.0
    last_run_at:         float | None       = None
    last_success_at:     float | None       = None
    last_failure_at:     float | None       = None
    last_error:          str | None         = None
    consecutive_failures: int               = 0
    snapshots_count:     int                = 0
    total_size_bytes:    int                = 0
    health:              BackupHealth       = BackupHealth.UNCONFIGURED
    health_message:      str               = "Backup not configured"
    estimated_size_bytes: int | None       = None
    # macOS Time Machine integration
    time_machine_enabled:        bool        = False
    time_machine_last_backup_at: float | None = None
    time_machine_destination:    str          = ""

    def to_dict(self) -> dict:
        return {
            "enabled": self.enabled,
            "running": self.running,
            "progress": self.progress,
            "last_run_at": self.last_run_at,
            "last_success_at": self.last_success_at,
            "last_failure_at": self.last_failure_at,
            "last_error": self.last_error,
            "consecutive_failures": self.consecutive_failures,
            "snapshots_count": self.snapshots_count,
            "total_size_bytes": self.total_size_bytes,
            "health": self.health,
            "health_message": self.health_message,
            "estimated_size_bytes": self.estimated_size_bytes,
            "time_machine_enabled": self.time_machine_enabled,
            "time_machine_last_backup_at": self.time_machine_last_backup_at,
            "time_machine_destination": self.time_machine_destination,
        }


@dataclass
class AppInventoryEntry:
    name:         str
    version:      str
    source:       str   # "dpkg", "rpm", "brew", "winget", "snap", "osquery"
    pkg_id:       str = ""
    install_date: str = ""

    def to_dict(self) -> dict:
        return {
            "name": self.name, "version": self.version,
            "source": self.source, "pkg_id": self.pkg_id,
            "install_date": self.install_date,
        }


# ---------------------------------------------------------------------------
# Platform excludes
# ---------------------------------------------------------------------------

LINUX_EXCLUDES = [
    "**/.cache", "**/node_modules", "**/__pycache__", "**/*.pyc",
    "**/.venv", "**/venv", "**/.tox", "**/dist", "**/build",
    "**/.git/objects", "**/target/debug", "**/target/release",  # Rust
    "/tmp", "/var/tmp", "/proc", "/sys", "/dev", "/run", "/mnt", "/media",
    "/var/cache", "/var/log", "/snap",
    "**/Steam/steamapps", "**/.wine/drive_c/windows",  # gaming
]

MACOS_EXCLUDES = [
    "~/Library/Caches", "~/Library/Logs", "~/Library/Saved Application State",
    "~/Library/Application Support/*/Cache",
    "~/Library/Application Support/*/Caches",
    "~/.cache", "**/node_modules", "**/__pycache__",
    "~/Library/Containers/*/Data/Library/Caches",
    "/Volumes",                # external drives
    "/System/Volumes",
    "~/.Trash",
    "**/Steam/steamapps",
]

WINDOWS_EXCLUDES = [
    "%LOCALAPPDATA%\\Temp",
    "%TEMP%",
    "%APPDATA%\\..\\Local\\Temp",
    "**\\node_modules",
    "**\\__pycache__",
    "C:\\Windows",
    "C:\\$Recycle.Bin",
    "C:\\pagefile.sys",
    "C:\\hiberfil.sys",
    "C:\\swapfile.sys",
    "**\\Steam\\steamapps",
]

FREEBSD_EXCLUDES = [
    "/tmp", "/var/tmp", "/proc", "/dev", "/media", "/mnt",
    "/var/cache", "**/node_modules", "**/__pycache__",
]

OPENBSD_EXCLUDES = FREEBSD_EXCLUDES + ["/altroot"]


def _platform_excludes() -> list[str]:
    sys_name = platform.system()
    if sys_name == "Linux":
        return list(LINUX_EXCLUDES)
    elif sys_name == "Darwin":
        return list(MACOS_EXCLUDES)
    elif sys_name == "Windows":
        return list(WINDOWS_EXCLUDES)
    elif sys_name == "FreeBSD":
        return list(FREEBSD_EXCLUDES)
    elif sys_name == "OpenBSD":
        return list(OPENBSD_EXCLUDES)
    return []


# ---------------------------------------------------------------------------
# Default include paths
# ---------------------------------------------------------------------------

def _default_include_paths() -> list[str]:
    sys_name = platform.system()
    if sys_name == "Windows":
        home = os.environ.get("USERPROFILE", "C:\\Users\\Default")
        return [home]
    else:
        home = str(Path.home())
        extras = []
        # Common data dirs outside home
        for p in ["/etc", "/var/lib/postgresql", "/var/lib/mysql"]:
            if Path(p).exists():
                extras.append(p)
        return [home] + extras


# ---------------------------------------------------------------------------
# App inventory
# ---------------------------------------------------------------------------

async def _collect_app_inventory_linux() -> list[AppInventoryEntry]:
    entries: list[AppInventoryEntry] = []

    # dpkg
    try:
        proc = await asyncio.create_subprocess_exec(
            "dpkg-query", "-W", "-f=${Package}\t${Version}\t${Status}\n",
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL,
        )
        out, _ = await proc.communicate()
        for line in out.decode(errors="replace").splitlines():
            parts = line.split("\t")
            if len(parts) >= 3 and "installed" in parts[2]:
                entries.append(AppInventoryEntry(
                    name=parts[0], version=parts[1], source="dpkg", pkg_id=parts[0],
                ))
    except FileNotFoundError:
        pass

    # rpm (if no dpkg)
    if not entries:
        try:
            proc = await asyncio.create_subprocess_exec(
                "rpm", "-qa", "--queryformat", "%{NAME}\t%{VERSION}-%{RELEASE}\n",
                stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL,
            )
            out, _ = await proc.communicate()
            for line in out.decode(errors="replace").splitlines():
                parts = line.split("\t")
                if len(parts) == 2:
                    entries.append(AppInventoryEntry(
                        name=parts[0], version=parts[1], source="rpm", pkg_id=parts[0],
                    ))
        except FileNotFoundError:
            pass

    # snap
    try:
        proc = await asyncio.create_subprocess_exec(
            "snap", "list", "--unicode=never",
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL,
        )
        out, _ = await proc.communicate()
        for line in out.decode(errors="replace").splitlines()[1:]:
            parts = line.split()
            if len(parts) >= 2:
                entries.append(AppInventoryEntry(
                    name=parts[0], version=parts[1], source="snap", pkg_id=parts[0],
                ))
    except FileNotFoundError:
        pass

    return entries


async def _collect_app_inventory_macos() -> list[AppInventoryEntry]:
    entries: list[AppInventoryEntry] = []
    try:
        proc = await asyncio.create_subprocess_exec(
            "brew", "list", "--versions",
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL,
        )
        out, _ = await proc.communicate()
        for line in out.decode(errors="replace").splitlines():
            parts = line.split()
            if len(parts) >= 2:
                entries.append(AppInventoryEntry(
                    name=parts[0], version=parts[1], source="brew", pkg_id=parts[0],
                ))
    except FileNotFoundError:
        pass
    return entries


async def _collect_app_inventory_windows() -> list[AppInventoryEntry]:
    entries: list[AppInventoryEntry] = []
    try:
        proc = await asyncio.create_subprocess_exec(
            "winget", "list", "--accept-source-agreements",
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL,
        )
        out, _ = await proc.communicate()
        for line in out.decode(errors="replace").splitlines()[3:]:
            parts = line.split()
            if len(parts) >= 2:
                entries.append(AppInventoryEntry(
                    name=" ".join(parts[:-2]), version=parts[-1], source="winget",
                ))
    except FileNotFoundError:
        pass
    return entries


async def collect_app_inventory() -> list[AppInventoryEntry]:
    """Collect installed applications using platform-native package managers."""
    sys_name = platform.system()
    if sys_name == "Linux":
        return await _collect_app_inventory_linux()
    elif sys_name == "Darwin":
        return await _collect_app_inventory_macos()
    elif sys_name == "Windows":
        return await _collect_app_inventory_windows()
    return []


# ---------------------------------------------------------------------------
# Pre-backup DB hooks
# ---------------------------------------------------------------------------

async def _run_db_hooks(dump_dir: Path) -> list[str]:
    """
    Detect running databases and dump them before backup.
    Returns list of dump file paths created.
    """
    dump_dir.mkdir(parents=True, exist_ok=True)
    dumps: list[str] = []

    # PostgreSQL
    try:
        proc = await asyncio.create_subprocess_exec(
            "pg_isready", stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL,
        )
        if await proc.wait() == 0:
            dump_path = dump_dir / f"pg_dump_{int(time.time())}.sql.gz"
            proc2 = await asyncio.create_subprocess_exec(
                "bash", "-c", f"pg_dumpall | gzip > {dump_path}",
                stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL,
            )
            await proc2.wait()
            if dump_path.exists():
                dumps.append(str(dump_path))
                log.info("PostgreSQL dump: %s", dump_path)
    except FileNotFoundError:
        pass

    # MySQL / MariaDB
    try:
        proc = await asyncio.create_subprocess_exec(
            "mysqladmin", "ping", "--silent",
            stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL,
        )
        if await proc.wait() == 0:
            dump_path = dump_dir / f"mysql_dump_{int(time.time())}.sql.gz"
            proc2 = await asyncio.create_subprocess_exec(
                "bash", "-c", f"mysqldump --all-databases | gzip > {dump_path}",
                stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL,
            )
            await proc2.wait()
            if dump_path.exists():
                dumps.append(str(dump_path))
                log.info("MySQL dump: %s", dump_path)
    except FileNotFoundError:
        pass

    return dumps


# ---------------------------------------------------------------------------
# Idle detection
# ---------------------------------------------------------------------------

async def detect_time_machine() -> dict:
    """
    Detect macOS Time Machine configuration via tmutil.

    Returns a dict with:
      enabled          bool   — whether TM is configured with a destination
      running          bool   — whether a backup is currently in progress
      last_backup_at   float | None — unix timestamp of last completed backup
      destination      str    — destination volume name / URL
      phase            str    — current phase from tmutil status (or "")

    On non-macOS systems always returns {"enabled": False}.
    """
    if platform.system() != "Darwin":
        return {"enabled": False, "running": False, "last_backup_at": None,
                "destination": "", "phase": ""}

    result: dict = {
        "enabled": False, "running": False,
        "last_backup_at": None, "destination": "", "phase": "",
    }

    import shutil as _sh
    if not _sh.which("tmutil"):
        return result

    async def _tmutil(*args: str) -> str:
        try:
            proc = await asyncio.create_subprocess_exec(
                "tmutil", *args,
                stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL,
            )
            out, _ = await asyncio.wait_for(proc.communicate(), timeout=10.0)
            return out.decode(errors="replace").strip()
        except Exception:
            return ""

    # Check if a destination is configured
    dest_out = await _tmutil("destinationinfo")
    if dest_out and "Name" in dest_out:
        result["enabled"] = True
        for line in dest_out.splitlines():
            if line.strip().startswith("Name"):
                parts = line.split(":", 1)
                if len(parts) == 2:
                    result["destination"] = parts[1].strip()
                    break

    if not result["enabled"]:
        return result

    # Current status
    status_out = await _tmutil("status")
    if status_out:
        if "Running = 1" in status_out:
            result["running"] = True
        for line in status_out.splitlines():
            if "BackupPhase" in line:
                parts = line.split("=", 1)
                if len(parts) == 2:
                    result["phase"] = parts[1].strip().strip('";')

    # Last completed backup timestamp
    latest_out = await _tmutil("latestbackup")
    if latest_out:
        # tmutil latestbackup returns a path like /Volumes/TM/Backups.backupdb/MacBook/2024-01-15-120000
        tail = latest_out.rstrip("/").split("/")[-1]
        # Try to parse YYYY-MM-DD-HHmmss format
        import re as _re
        m = _re.match(r"(\d{4})-(\d{2})-(\d{2})-(\d{2})(\d{2})(\d{2})", tail)
        if m:
            import datetime as _dt
            try:
                dt = _dt.datetime(
                    int(m.group(1)), int(m.group(2)), int(m.group(3)),
                    int(m.group(4)), int(m.group(5)), int(m.group(6)),
                )
                result["last_backup_at"] = dt.timestamp()
            except ValueError:
                pass

    return result


async def _idle_seconds() -> float:
    """Return number of seconds since last user input (best effort)."""
    sys_name = platform.system()
    if sys_name == "Linux":
        try:
            # xprintidle returns milliseconds
            proc = await asyncio.create_subprocess_exec(
                "xprintidle", stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL,
            )
            out, _ = await proc.communicate()
            return float(out.strip()) / 1000.0
        except (FileNotFoundError, ValueError):
            pass
    elif sys_name == "Darwin":
        try:
            proc = await asyncio.create_subprocess_exec(
                "ioreg", "-c", "IOHIDSystem",
                stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL,
            )
            out, _ = await proc.communicate()
            for line in out.decode().splitlines():
                if "HIDIdleTime" in line:
                    # Value is in nanoseconds
                    val = int(line.split("=")[-1].strip())
                    return val / 1e9
        except (FileNotFoundError, ValueError):
            pass
    return 999.0  # assume idle if we can't tell


async def _is_on_battery() -> bool:
    """Return True if running on battery with < 20% charge."""
    sys_name = platform.system()
    if sys_name == "Linux":
        for bat in Path("/sys/class/power_supply").glob("BAT*"):
            try:
                status = (bat / "status").read_text().strip()
                capacity = int((bat / "capacity").read_text().strip())
                if status == "Discharging" and capacity < 20:
                    return True
            except Exception:
                pass
    elif sys_name == "Darwin":
        try:
            proc = await asyncio.create_subprocess_exec(
                "pmset", "-g", "batt",
                stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL,
            )
            out, _ = await proc.communicate()
            text = out.decode()
            if "discharging" in text.lower():
                import re
                m = re.search(r"(\d+)%", text)
                if m and int(m.group(1)) < 20:
                    return True
        except FileNotFoundError:
            pass
    return False


# ---------------------------------------------------------------------------
# Restic helpers
# ---------------------------------------------------------------------------

def _restic_repo_url(cfg: BackupConfig) -> str:
    """Build the RESTIC_REPOSITORY string for the given destination."""
    dc = cfg.destination_config
    d = cfg.destination
    if d == BackupDestination.LOCAL:
        return dc.get("path", str(Path.home() / ".ozma-backup"))
    elif d in (BackupDestination.S3, BackupDestination.CONNECT_S3):
        endpoint = dc.get("endpoint", "")
        bucket = dc.get("bucket", "ozma-backup")
        prefix = dc.get("prefix", "")
        if endpoint:
            return f"s3:{endpoint}/{bucket}/{prefix}".rstrip("/")
        return f"s3:s3.amazonaws.com/{bucket}/{prefix}".rstrip("/")
    elif d == BackupDestination.SFTP:
        user = dc.get("user", "")
        host = dc.get("host", "")
        path = dc.get("path", "/backup")
        return f"sftp:{user}@{host}:{path}" if user else f"sftp:{host}:{path}"
    elif d == BackupDestination.REST:
        return f"rest:{dc.get('url', 'http://localhost:8000/')}"
    return str(Path.home() / ".ozma-backup")


def _restic_env(cfg: BackupConfig, password: str) -> dict[str, str]:
    """Build the environment for restic subprocesses."""
    env = {**os.environ, "RESTIC_REPOSITORY": _restic_repo_url(cfg), "RESTIC_PASSWORD": password}
    dc = cfg.destination_config
    d = cfg.destination
    if d in (BackupDestination.S3, BackupDestination.CONNECT_S3):
        if dc.get("access_key_id"):
            env["AWS_ACCESS_KEY_ID"] = dc["access_key_id"]
        if dc.get("secret_access_key"):
            env["AWS_SECRET_ACCESS_KEY"] = dc["secret_access_key"]
    elif d == BackupDestination.SFTP and dc.get("identity_file"):
        env["RESTIC_SSH_COMMAND"] = f"ssh -i {dc['identity_file']}"
    return env


# ---------------------------------------------------------------------------
# BackupManager
# ---------------------------------------------------------------------------

class BackupManager:
    """
    Manages Restic-backed backups for an Ozma agent node.

    Lifecycle:
        await mgr.start()   — loads config, schedules backup loop
        await mgr.stop()    — cancels tasks
        await mgr.run_backup() — manual run (or called by scheduler)
    """

    # Health thresholds (seconds)
    GREEN_THRESHOLD  = 3  * 24 * 3600   # < 3 days → green
    YELLOW_THRESHOLD = 7  * 24 * 3600   # < 7 days → yellow
    ORANGE_THRESHOLD = 14 * 24 * 3600   # < 14 days → orange
    # ≥ 14 days → red

    def __init__(
        self,
        data_dir: Path | None = None,
        key_store: Any | None = None,
    ) -> None:
        if data_dir is None:
            data_dir = Path.home() / ".ozma" / "backup"
        self._data_dir = data_dir
        self._key_store = key_store
        self._config = BackupConfig()
        self._status = BackupStatus()
        self._tasks: list[asyncio.Task] = []
        self._lock = asyncio.Lock()
        self._last_verify_at: float = 0.0
        self._last_inventory_at: float = 0.0
        self._app_inventory: list[AppInventoryEntry] = []

    # ------------------------------------------------------------------
    # Lifecycle
    # ------------------------------------------------------------------

    async def start(self) -> None:
        self._data_dir.mkdir(parents=True, exist_ok=True)
        self._load()
        self._status.enabled = self._config.enabled
        self._update_health()
        if platform.system() == "Darwin":
            try:
                await self.refresh_time_machine_status()
            except Exception:
                pass
        self._tasks = [
            asyncio.create_task(self._schedule_loop(), name="backup:scheduler"),
            asyncio.create_task(self._verify_loop(), name="backup:verify"),
        ]
        log.info("BackupManager started (enabled=%s, mode=%s, dest=%s)",
                 self._config.enabled, self._config.mode, self._config.destination)

    async def stop(self) -> None:
        for t in self._tasks:
            t.cancel()
        await asyncio.gather(*self._tasks, return_exceptions=True)
        self._tasks.clear()

    # ------------------------------------------------------------------
    # Configuration
    # ------------------------------------------------------------------

    def get_config(self) -> BackupConfig:
        return self._config

    def set_config(self, **updates) -> BackupConfig:
        for k, v in updates.items():
            if k == "retention" and isinstance(v, dict):
                self._config.retention = RetentionPolicy.from_dict(v)
            elif k == "mode":
                self._config.mode = BackupMode(v)
            elif k == "destination":
                self._config.destination = BackupDestination(v)
            elif hasattr(self._config, k):
                setattr(self._config, k, v)
        self._config.enabled = self._config.enabled  # refresh
        self._status.enabled = self._config.enabled
        self._save()
        return self._config

    def dismiss_alert(self, days: int = 30) -> None:
        self._config.alert_dismissed_until = time.time() + days * 86400
        self._save()

    # ------------------------------------------------------------------
    # Manual operations
    # ------------------------------------------------------------------

    # ------------------------------------------------------------------
    # Onboarding / default-on
    # ------------------------------------------------------------------

    def get_onboarding_config(self) -> BackupConfig:
        """
        Return a sensible default BackupConfig for first-run onboarding.

        The config is not enabled — the user must explicitly opt in via
        the dashboard. It defaults to smart mode with local destination
        so the user can preview what would be backed up before committing.
        """
        return BackupConfig(
            enabled=False,
            mode=BackupMode.SMART,
            destination=BackupDestination.LOCAL,
            destination_config={
                "path": str(Path.home() / ".ozma" / "backup_store"),
            },
            encrypt=True,
            schedule="adaptive",
        )

    async def is_default_on_eligible(self) -> bool:
        """
        Return True if this machine is a good candidate for auto-enabling
        backup on first start.

        Criteria:
          - Not already configured
          - Has at least 10 GB free on the home directory filesystem
          - Not on battery (laptops — avoid kicking off a large backup on
            battery at first install)
        """
        if self._config.enabled:
            return False
        try:
            import shutil as _sh
            usage = _sh.disk_usage(str(Path.home()))
            if usage.free < 10 * 1024 ** 3:
                return False
        except Exception:
            return False
        if await _is_on_battery():
            return False
        return True

    async def refresh_time_machine_status(self) -> None:
        """Update Time Machine fields in _status (macOS only, no-op elsewhere)."""
        tm = await detect_time_machine()
        self._status.time_machine_enabled = tm.get("enabled", False)
        self._status.time_machine_destination = tm.get("destination", "")
        ts = tm.get("last_backup_at")
        self._status.time_machine_last_backup_at = ts

    async def run_backup(self, mode: BackupMode | None = None) -> BackupStatus:
        """
        Run a backup immediately.  Returns updated status.
        Thread-safe — concurrent calls serialise on the lock.
        """
        if self._lock.locked():
            log.debug("Backup already running — skipping concurrent request")
            return self._status

        async with self._lock:
            effective_mode = mode or self._config.mode
            self._status.running = True
            self._status.progress = 0.0
            start_ts = time.time()
            log.info("Backup started (mode=%s, dest=%s)", effective_mode, self._config.destination)

            try:
                password = await self._get_password()
                env = _restic_env(self._config, password)

                # Ensure repo is initialised
                await self._ensure_repo_init(env)

                # Pre-backup DB hooks
                dump_paths: list[str] = []
                if self._config.db_hooks:
                    dump_dir = self._data_dir / "db_dumps"
                    dump_paths = await _run_db_hooks(dump_dir)

                # App inventory snapshot
                self._status.progress = 0.1
                await self._refresh_app_inventory()
                await self._save_app_snapshot()

                # Build backup arguments
                args = await self._build_backup_args(effective_mode, dump_paths)

                # ZFS/BTRFS snapshot for disk_image mode
                if effective_mode == BackupMode.DISK_IMAGE:
                    args = await self._disk_image_args()
                else:
                    args = await self._files_args(effective_mode, dump_paths)

                self._status.progress = 0.2
                rc, out, err = await self._restic("backup", *args, env=env)
                if rc != 0:
                    raise RuntimeError(f"restic backup failed (rc={rc}): {err[:500]}")

                # Prune after backup
                self._status.progress = 0.8
                await self._prune(env)

                # Update status
                self._status.last_success_at = time.time()
                self._status.last_run_at = self._status.last_success_at
                self._status.consecutive_failures = 0
                self._status.last_error = None
                elapsed = time.time() - start_ts
                log.info("Backup completed in %.1fs", elapsed)

                # Refresh snapshot count + repo size
                await self._refresh_stats(env)

            except Exception as exc:
                self._status.last_failure_at = time.time()
                self._status.last_run_at = self._status.last_failure_at
                self._status.consecutive_failures += 1
                self._status.last_error = str(exc)
                log.error("Backup failed: %s", exc)
            finally:
                self._status.running = False
                self._status.progress = None
                self._update_health()
                self._save_status()

        return self._status

    async def list_snapshots(self) -> list[dict]:
        """Return a list of Restic snapshots (JSON)."""
        password = await self._get_password()
        env = _restic_env(self._config, password)
        rc, out, err = await self._restic("snapshots", "--json", env=env)
        if rc != 0:
            return []
        try:
            return json.loads(out) or []
        except json.JSONDecodeError:
            return []

    async def list_snapshot_files(self, snapshot_id: str, path: str = "/") -> list[dict]:
        """List files within a snapshot at the given path."""
        password = await self._get_password()
        env = _restic_env(self._config, password)
        rc, out, err = await self._restic("ls", "--json", snapshot_id, path, env=env)
        if rc != 0:
            return []
        entries = []
        for line in out.splitlines():
            try:
                entries.append(json.loads(line))
            except json.JSONDecodeError:
                pass
        return entries

    async def restore(
        self,
        snapshot_id: str = "latest",
        source_path: str = "/",
        target_path: str | None = None,
    ) -> bool:
        """
        Restore files from a snapshot.

        snapshot_id : Restic snapshot ID or "latest"
        source_path : Path within the snapshot to restore
        target_path : Where to restore to; defaults to original location
        """
        password = await self._get_password()
        env = _restic_env(self._config, password)
        args = [snapshot_id, "--include", source_path]
        if target_path:
            args += ["--target", target_path]
        else:
            args += ["--target", "/"]
        rc, _, err = await self._restic("restore", *args, env=env)
        if rc != 0:
            log.error("Restore failed: %s", err[:200])
            return False
        log.info("Restore completed: snapshot=%s path=%s → %s",
                 snapshot_id, source_path, target_path or "(original)")
        return True

    async def verify(self) -> bool:
        """Run restic check + test-restore of a sample file."""
        password = await self._get_password()
        env = _restic_env(self._config, password)
        rc, out, err = await self._restic("check", "--read-data-subset=5%", env=env)
        ok = rc == 0
        self._last_verify_at = time.time()
        if ok:
            log.info("Backup verify passed")
        else:
            log.warning("Backup verify failed: %s", err[:200])
        return ok

    async def estimate_size(self) -> int:
        """Estimate backup size by scanning include paths (bytes)."""
        include = _default_include_paths() if not self._config.include_paths \
                  else self._config.include_paths
        total = 0
        loop = asyncio.get_event_loop()

        def _scan():
            size = 0
            for base in include:
                p = Path(base)
                if not p.exists():
                    continue
                try:
                    for item in p.rglob("*"):
                        try:
                            if item.is_file(follow_symlinks=False):
                                size += item.stat().st_size
                        except (OSError, PermissionError):
                            pass
                except (OSError, PermissionError):
                    pass
            return size

        try:
            total = await asyncio.wait_for(
                loop.run_in_executor(None, _scan), timeout=30.0
            )
        except asyncio.TimeoutError:
            pass
        self._status.estimated_size_bytes = total
        return total

    # ------------------------------------------------------------------
    # App inventory
    # ------------------------------------------------------------------

    async def get_app_inventory(self) -> list[AppInventoryEntry]:
        if not self._app_inventory:
            await self._refresh_app_inventory()
        return self._app_inventory

    async def restore_apps(self, app_names: list[str]) -> dict[str, str]:
        """
        Attempt to reinstall applications from the inventory.
        Returns dict of {name: "installed" | "failed" | "manual"}.
        """
        results: dict[str, str] = {}
        inventory = {e.name: e for e in await self.get_app_inventory()}

        for name in app_names:
            entry = inventory.get(name)
            if not entry:
                results[name] = "not_found"
                continue
            success = await self._reinstall_package(entry)
            results[name] = "installed" if success else "manual"
        return results

    async def _reinstall_package(self, entry: AppInventoryEntry) -> bool:
        cmds: dict[str, list[str]] = {
            "dpkg": ["apt-get", "install", "-y", entry.pkg_id or entry.name],
            "rpm": ["dnf", "install", "-y", entry.pkg_id or entry.name],
            "brew": ["brew", "install", entry.pkg_id or entry.name],
            "snap": ["snap", "install", entry.pkg_id or entry.name],
            "winget": ["winget", "install", "-e", "--id", entry.pkg_id or entry.name,
                       "--accept-source-agreements", "--accept-package-agreements"],
        }
        cmd = cmds.get(entry.source)
        if not cmd:
            return False
        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL,
        )
        return await proc.wait() == 0

    # ------------------------------------------------------------------
    # Status
    # ------------------------------------------------------------------

    def get_status(self) -> BackupStatus:
        return self._status

    # ------------------------------------------------------------------
    # Internal: scheduling
    # ------------------------------------------------------------------

    async def _schedule_loop(self) -> None:
        """Adaptive scheduler — wakes every 15 minutes and checks conditions."""
        while True:
            try:
                await asyncio.sleep(15 * 60)
                if not self._config.enabled:
                    continue
                if self._config.schedule == "adaptive":
                    if await self._should_run_now():
                        await self.run_backup()
                else:
                    # Cron schedule: basic support for "daily", "weekly", "hourly"
                    if self._config.schedule == "daily" and self._hours_since_success() >= 24:
                        await self.run_backup()
                    elif self._config.schedule == "weekly" and self._hours_since_success() >= 168:
                        await self.run_backup()
            except asyncio.CancelledError:
                break
            except Exception:
                log.exception("Backup scheduler error")

    async def _should_run_now(self) -> bool:
        """Return True if all adaptive conditions are met."""
        # Don't run if already recent
        if self._hours_since_success() < 24:
            return False
        # Idle check
        idle = await _idle_seconds()
        if idle < 300:  # 5 minutes
            return False
        # Battery check
        if await _is_on_battery():
            return False
        return True

    async def _verify_loop(self) -> None:
        """Weekly verification — run restic check on Sunday."""
        while True:
            try:
                await asyncio.sleep(3600)  # check hourly
                if not self._config.enabled or not self._config.verify_weekly:
                    continue
                week_ago = time.time() - 7 * 24 * 3600
                if self._last_verify_at < week_ago and not self._lock.locked():
                    await self.verify()
            except asyncio.CancelledError:
                break
            except Exception:
                log.exception("Backup verify loop error")

    def _hours_since_success(self) -> float:
        if self._status.last_success_at is None:
            return 999.0
        return (time.time() - self._status.last_success_at) / 3600

    # ------------------------------------------------------------------
    # Internal: backup pipeline
    # ------------------------------------------------------------------

    async def _files_args(self, mode: BackupMode, extra_paths: list[str]) -> list[str]:
        """Build restic backup arguments for files/smart/advanced modes."""
        include = list(self._config.include_paths) or _default_include_paths()
        include += extra_paths

        args: list[str] = list(include)

        # Excludes
        excludes = _platform_excludes() + list(self._config.extra_excludes)

        if mode == BackupMode.SMART:
            # Exclude package-managed app directories
            installed_dirs = await self._installed_app_dirs()
            excludes += installed_dirs

        for exc in excludes:
            args += ["--exclude", exc]

        # Windows VSS
        if platform.system() == "Windows" and self._config.use_vss:
            args.append("--use-fs-snapshot")

        # Business backup flag
        if self._config.append_only:
            args.append("--no-lock")

        # Bandwidth limit
        bw = self._config.bandwidth_limit
        if bw > 0:
            args += ["--limit-upload", str(bw)]
        elif bw == 0:
            # Auto-detect: limit to 80% of available bandwidth
            args += ["--limit-upload", "0"]

        # Tag with hostname and timestamp
        args += ["--tag", f"host:{platform.node()}", "--tag", f"mode:{mode.value}"]

        return args

    async def _disk_image_args(self) -> list[str]:
        """
        For disk_image mode: run partclone to a temp file, back that up.
        Returns args for restic backup pointing at the image file.
        """
        image_dir = self._data_dir / "disk_images"
        image_dir.mkdir(parents=True, exist_ok=True)

        sys_name = platform.system()

        # ZFS send — FreeBSD and Linux (OpenZFS)
        if sys_name in ("FreeBSD", "Linux"):
            zpool = await self._detect_zpool()
            if zpool:
                image_path = image_dir / f"zfs_send_{int(time.time())}.zfs"
                with open(image_path, "wb") as fp:
                    proc = await asyncio.create_subprocess_exec(
                        "zfs", "send", "-R", zpool,
                        stdout=fp,
                        stderr=asyncio.subprocess.DEVNULL,
                    )
                await proc.wait()
                if proc.returncode == 0 and image_path.stat().st_size > 0:
                    return [str(image_path), "--tag", "disk_image:zfs_send"]
                image_path.unlink(missing_ok=True)

        # BTRFS send — Linux only
        if sys_name == "Linux":
            subvol = await self._detect_btrfs_subvol()
            if subvol:
                # Create a read-only snapshot then stream it
                snap_path = f"{subvol}/.ozma_snap_{int(time.time())}"
                snap_proc = await asyncio.create_subprocess_exec(
                    "btrfs", "subvolume", "snapshot", "-r", subvol, snap_path,
                    stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL,
                )
                await snap_proc.wait()
                if snap_proc.returncode == 0:
                    image_path = image_dir / f"btrfs_send_{int(time.time())}.btrfs"
                    with open(image_path, "wb") as fp:
                        send_proc = await asyncio.create_subprocess_exec(
                            "btrfs", "send", snap_path,
                            stdout=fp,
                            stderr=asyncio.subprocess.DEVNULL,
                        )
                    await send_proc.wait()
                    # Clean up the snapshot regardless of outcome
                    del_proc = await asyncio.create_subprocess_exec(
                        "btrfs", "subvolume", "delete", snap_path,
                        stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL,
                    )
                    await del_proc.wait()
                    if send_proc.returncode == 0 and image_path.stat().st_size > 0:
                        return [str(image_path), "--tag", "disk_image:btrfs_send"]
                    image_path.unlink(missing_ok=True)

        # Linux/generic: partclone of root
        try:
            root_dev = await self._detect_root_device()
            if root_dev:
                image_path = image_dir / f"partclone_{int(time.time())}.img"
                proc = await asyncio.create_subprocess_exec(
                    "partclone.auto", "-c", "-s", root_dev, "-O", str(image_path),
                    stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL,
                )
                await proc.wait()
                return [str(image_path), "--tag", "disk_image:partclone"]
        except FileNotFoundError:
            log.warning("partclone not found — falling back to dd image")

        return await self._files_args(BackupMode.FILES, [])

    async def _build_backup_args(self, mode: BackupMode, extra_paths: list[str]) -> list[str]:
        if mode == BackupMode.DISK_IMAGE:
            return await self._disk_image_args()
        return await self._files_args(mode, extra_paths)

    async def _installed_app_dirs(self) -> list[str]:
        """
        Return directories that belong to package-managed apps (safe to exclude).
        Smarter than pattern matching — uses actual installed package file list.
        """
        dirs: set[str] = set()
        sys_name = platform.system()
        home = str(Path.home())

        if sys_name == "Darwin":
            # Homebrew installs to /opt/homebrew or /usr/local
            for prefix in ["/opt/homebrew", "/usr/local/Cellar", "/usr/local/opt"]:
                if Path(prefix).exists():
                    dirs.add(prefix)
            # .app bundles in /Applications
            dirs.add("/Applications")

        elif sys_name == "Linux":
            # dpkg owns /usr, /lib etc. — exclude those, keep /home
            for d in ["/usr", "/lib", "/lib64", "/opt"]:
                if Path(d).exists():
                    dirs.add(d)

        return list(dirs)

    async def _detect_root_device(self) -> str | None:
        try:
            proc = await asyncio.create_subprocess_exec(
                "findmnt", "-n", "-o", "SOURCE", "/",
                stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL,
            )
            out, _ = await proc.communicate()
            return out.decode().strip() or None
        except FileNotFoundError:
            return None

    async def _detect_btrfs_subvol(self) -> str | None:
        """Return the BTRFS root subvolume path if the root FS is BTRFS, else None."""
        import shutil as _sh
        if not _sh.which("btrfs"):
            return None
        try:
            proc = await asyncio.create_subprocess_exec(
                "findmnt", "-n", "-o", "FSTYPE,SOURCE,TARGET", "/",
                stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL,
            )
            out, _ = await proc.communicate()
            line = out.decode().strip()
            if line.startswith("btrfs"):
                parts = line.split()
                return parts[2] if len(parts) >= 3 else "/"
        except FileNotFoundError:
            pass
        return None

    async def _detect_zpool(self) -> str | None:
        try:
            proc = await asyncio.create_subprocess_exec(
                "zpool", "list", "-H", "-o", "name",
                stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL,
            )
            out, _ = await proc.communicate()
            pools = out.decode().strip().splitlines()
            return pools[0] if pools else None
        except FileNotFoundError:
            return None

    async def _ensure_repo_init(self, env: dict) -> None:
        """Initialise the Restic repo if it doesn't exist yet."""
        rc, _, _ = await self._restic("snapshots", env=env)
        if rc != 0:
            log.info("Initialising Restic repository")
            init_args = []
            if self._config.append_only:
                init_args = ["--repository-version", "2"]
            rc2, _, err = await self._restic("init", *init_args, env=env)
            if rc2 != 0:
                raise RuntimeError(f"restic init failed: {err[:200]}")

    async def _prune(self, env: dict) -> None:
        forget_args = self._config.retention.to_forget_args() + ["--prune"]
        rc, _, err = await self._restic("forget", *forget_args, env=env)
        if rc != 0:
            log.warning("restic forget/prune failed: %s", err[:100])

    async def _refresh_stats(self, env: dict) -> None:
        rc, out, _ = await self._restic("stats", "--json", env=env)
        if rc == 0:
            try:
                stats = json.loads(out)
                self._status.snapshots_count = stats.get("snapshots_count", 0)
                self._status.total_size_bytes = stats.get("total_size", 0)
            except json.JSONDecodeError:
                pass

    async def _refresh_app_inventory(self) -> None:
        if time.time() - self._last_inventory_at < 3600:
            return  # cache for 1 hour
        try:
            self._app_inventory = await collect_app_inventory()
            self._last_inventory_at = time.time()
        except Exception as exc:
            log.warning("App inventory collection failed: %s", exc)

    async def _save_app_snapshot(self) -> None:
        snapshot_path = self._data_dir / "apps_snapshot.json"
        loop = asyncio.get_event_loop()
        data = [e.to_dict() for e in self._app_inventory]

        def _write():
            tmp = snapshot_path.with_suffix(".tmp")
            tmp.write_text(json.dumps(data, indent=2))
            tmp.rename(snapshot_path)

        await loop.run_in_executor(None, _write)

    # ------------------------------------------------------------------
    # Password / key management
    # ------------------------------------------------------------------

    async def _get_password(self) -> str:
        """
        Get the Restic repository password.

        For ZK destinations (connect_s3) or when encrypt=True and a key_store
        is available, derives from key_store.derive_subkey("backup").
        Otherwise falls back to a locally stored password.
        """
        if self._key_store and self._config.encrypt:
            try:
                subkey = self._key_store.derive_subkey("backup")
                return subkey.hex()  # 64-char hex → Restic password
            except Exception as exc:
                log.warning("Key store unavailable (%s) — using local password", exc)

        # Fallback: local password file (or generate once)
        pw_path = self._data_dir / ".restic_password"
        if pw_path.exists():
            return pw_path.read_text().strip()
        pw = os.urandom(32).hex()
        pw_path.parent.mkdir(parents=True, exist_ok=True)
        pw_path.write_text(pw)
        pw_path.chmod(0o600)
        return pw

    # ------------------------------------------------------------------
    # Restic subprocess
    # ------------------------------------------------------------------

    async def _restic(self, subcmd: str, *args: str, env: dict | None = None) -> tuple[int, str, str]:
        """Run a restic command.  Returns (returncode, stdout, stderr)."""
        restic_bin = shutil.which("restic") or "restic"
        cmd = [restic_bin, subcmd, *args]
        try:
            proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
                env=env,
            )
            out_bytes, err_bytes = await proc.communicate()
            return proc.returncode or 0, out_bytes.decode(errors="replace"), err_bytes.decode(errors="replace")
        except FileNotFoundError:
            return 1, "", "restic binary not found — install restic"
        except Exception as exc:
            return 1, "", str(exc)

    # ------------------------------------------------------------------
    # Health
    # ------------------------------------------------------------------

    def _update_health(self) -> None:
        if not self._config.enabled:
            self._status.health = BackupHealth.UNCONFIGURED
            self._status.health_message = "Backup not configured"
            return

        now = time.time()
        last = self._status.last_success_at
        failures = self._status.consecutive_failures

        if failures >= 3:
            self._status.health = BackupHealth.RED
            self._status.health_message = f"{failures} consecutive failures"
        elif last is None:
            self._status.health = BackupHealth.YELLOW
            self._status.health_message = "No backup completed yet"
        else:
            age = now - last
            if failures >= 2:
                self._status.health = BackupHealth.ORANGE
                self._status.health_message = f"{failures} consecutive failures"
            elif age > self.ORANGE_THRESHOLD:
                self._status.health = BackupHealth.RED
                self._status.health_message = f"Last backup {int(age/86400)}d ago"
            elif age > self.YELLOW_THRESHOLD:
                self._status.health = BackupHealth.ORANGE
                self._status.health_message = f"Last backup {int(age/86400)}d ago"
            elif age > self.GREEN_THRESHOLD:
                self._status.health = BackupHealth.YELLOW
                self._status.health_message = f"Last backup {int(age/86400)}d ago"
            else:
                self._status.health = BackupHealth.GREEN
                self._status.health_message = "Backup current"

    # ------------------------------------------------------------------
    # Persistence
    # ------------------------------------------------------------------

    @property
    def _config_path(self) -> Path:
        return self._data_dir / "backup_config.json"

    @property
    def _status_path(self) -> Path:
        return self._data_dir / "backup_status.json"

    def _save(self) -> None:
        self._data_dir.mkdir(parents=True, exist_ok=True)
        tmp = self._config_path.with_suffix(".tmp")
        raw = {**self._config.to_dict(), "destination_config": self._config.destination_config}
        tmp.write_text(json.dumps(raw, indent=2))
        tmp.chmod(0o600)
        tmp.rename(self._config_path)

    def _load(self) -> None:
        if self._config_path.exists():
            try:
                self._config = BackupConfig.from_dict(
                    json.loads(self._config_path.read_text())
                )
            except Exception:
                log.exception("Failed to load backup config")
        if self._status_path.exists():
            try:
                d = json.loads(self._status_path.read_text())
                self._status.last_success_at  = d.get("last_success_at")
                self._status.last_failure_at  = d.get("last_failure_at")
                self._status.last_run_at      = d.get("last_run_at")
                self._status.last_error       = d.get("last_error")
                self._status.consecutive_failures = d.get("consecutive_failures", 0)
                self._status.snapshots_count  = d.get("snapshots_count", 0)
                self._status.total_size_bytes = d.get("total_size_bytes", 0)
            except Exception:
                log.exception("Failed to load backup status")

    def _save_status(self) -> None:
        self._data_dir.mkdir(parents=True, exist_ok=True)
        tmp = self._status_path.with_suffix(".tmp")
        tmp.write_text(json.dumps({
            "last_success_at":     self._status.last_success_at,
            "last_failure_at":     self._status.last_failure_at,
            "last_run_at":         self._status.last_run_at,
            "last_error":          self._status.last_error,
            "consecutive_failures": self._status.consecutive_failures,
            "snapshots_count":     self._status.snapshots_count,
            "total_size_bytes":    self._status.total_size_bytes,
        }, indent=2))
        tmp.rename(self._status_path)
