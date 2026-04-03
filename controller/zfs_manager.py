# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""ZFS pool/dataset/snapshot management for Ozma controller.

Supports:
- Dataset creation and management for file shares
- Automatic snapshot scheduling (hourly/daily/weekly) with retention
- Samba shadow_copy2-compatible snapshot naming (@GMT-...) for Windows
  Previous Versions support
- zfs send incremental streaming to Ozma Connect for cloud backup
- ZFS native encryption for zero-knowledge cloud backup
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import time
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.zfs")

# ZFS snapshot label format. ZFS snapshot names cannot contain '@', so we
# use "GMT-..." without the leading '@'. Samba's shadow_copy2 module is
# configured with shadow:format = GMT-%Y.%m.%d-%H.%M.%S to match.
# In .zfs/snapshot/ the directory appears as "GMT-YYYY.MM.DD-HH.MM.SS".
# We store short_name with '@' prefix (e.g. "@GMT-...") for API clarity.
_GMT_FMT = "GMT-%Y.%m.%d-%H.%M.%S"


# ---------------------------------------------------------------------------
# Data models
# ---------------------------------------------------------------------------

@dataclass
class ZFSDataset:
    name: str           # e.g. "tank/shares/homes"
    pool: str           # e.g. "tank"
    mountpoint: str     # e.g. "/tank/shares/homes"
    used_bytes: int = 0
    avail_bytes: int = 0
    refer_bytes: int = 0
    encrypted: bool = False
    compression: str = "lz4"
    quota: int = 0      # bytes, 0 = none

    def to_dict(self) -> dict:
        return {
            "name": self.name,
            "pool": self.pool,
            "mountpoint": self.mountpoint,
            "used_bytes": self.used_bytes,
            "avail_bytes": self.avail_bytes,
            "refer_bytes": self.refer_bytes,
            "encrypted": self.encrypted,
            "compression": self.compression,
            "quota": self.quota,
        }


@dataclass
class ZFSSnapshot:
    name: str           # e.g. "tank/shares/homes@@GMT-2024.01.15-02.00.00"
    dataset: str        # e.g. "tank/shares/homes"
    short_name: str     # e.g. "@GMT-2024.01.15-02.00.00"
    created: float      # unix timestamp
    used_bytes: int = 0

    def to_dict(self) -> dict:
        return {
            "name": self.name,
            "dataset": self.dataset,
            "short_name": self.short_name,
            "created": self.created,
            "used_bytes": self.used_bytes,
        }


@dataclass
class SnapshotPolicy:
    """Retention policy for automatic snapshots."""
    hourly: int = 24    # keep last N hourly snapshots
    daily: int = 30     # keep last N daily snapshots
    weekly: int = 52    # keep last N weekly snapshots
    monthly: int = 12   # keep last N monthly snapshots

    def to_dict(self) -> dict:
        return {
            "hourly": self.hourly,
            "daily": self.daily,
            "weekly": self.weekly,
            "monthly": self.monthly,
        }

    @classmethod
    def from_dict(cls, d: dict) -> SnapshotPolicy:
        return cls(
            hourly=d.get("hourly", 24),
            daily=d.get("daily", 30),
            weekly=d.get("weekly", 52),
            monthly=d.get("monthly", 12),
        )


@dataclass
class ZFSManagedDataset:
    """A dataset managed by Ozma — tracks snapshot policy and backup state."""
    dataset: str
    auto_snapshot: bool = True
    policy: SnapshotPolicy = field(default_factory=SnapshotPolicy)
    # Last snapshot successfully sent to Connect (for incremental sends)
    last_sent_snapshot: str | None = None
    # ISO timestamp of last successful send
    last_sent_at: float | None = None
    # Cloud backup enabled
    cloud_backup: bool = False

    def to_dict(self) -> dict:
        return {
            "dataset": self.dataset,
            "auto_snapshot": self.auto_snapshot,
            "policy": self.policy.to_dict(),
            "last_sent_snapshot": self.last_sent_snapshot,
            "last_sent_at": self.last_sent_at,
            "cloud_backup": self.cloud_backup,
        }

    @classmethod
    def from_dict(cls, d: dict) -> ZFSManagedDataset:
        return cls(
            dataset=d["dataset"],
            auto_snapshot=d.get("auto_snapshot", True),
            policy=SnapshotPolicy.from_dict(d.get("policy", {})),
            last_sent_snapshot=d.get("last_sent_snapshot"),
            last_sent_at=d.get("last_sent_at"),
            cloud_backup=d.get("cloud_backup", False),
        )


@dataclass
class ZFSConfig:
    enabled: bool = False
    # Snapshot schedule intervals in seconds (0 = disabled)
    hourly_interval: int = 3600
    daily_interval: int = 86400
    weekly_interval: int = 604800
    monthly_interval: int = 2592000
    # Default encryption for new datasets
    default_encryption: bool = True
    # Connect backup endpoint (set by connect.py on auth)
    connect_backup_url: str | None = None

    def to_dict(self) -> dict:
        return {
            "enabled": self.enabled,
            "hourly_interval": self.hourly_interval,
            "daily_interval": self.daily_interval,
            "weekly_interval": self.weekly_interval,
            "monthly_interval": self.monthly_interval,
            "default_encryption": self.default_encryption,
        }

    @classmethod
    def from_dict(cls, d: dict) -> ZFSConfig:
        return cls(
            enabled=d.get("enabled", False),
            hourly_interval=d.get("hourly_interval", 3600),
            daily_interval=d.get("daily_interval", 86400),
            weekly_interval=d.get("weekly_interval", 604800),
            monthly_interval=d.get("monthly_interval", 2592000),
            default_encryption=d.get("default_encryption", True),
        )


# ---------------------------------------------------------------------------
# Manager
# ---------------------------------------------------------------------------

class ZFSManager:
    STATE_PATH = Path("/var/lib/ozma/zfs_state.json")

    def __init__(self, state_path: Path | None = None, event_queue=None) -> None:
        self._state_path = state_path or self.STATE_PATH
        self._config = ZFSConfig()
        # Datasets managed by Ozma (subset of all datasets on the system)
        self._managed: dict[str, ZFSManagedDataset] = {}
        self._task: asyncio.Task | None = None
        self._event_queue = event_queue
        self._last_hourly: float = 0.0
        self._last_daily: float = 0.0
        self._last_weekly: float = 0.0
        self._last_monthly: float = 0.0
        self._load()

    # ------------------------------------------------------------------
    # Lifecycle
    # ------------------------------------------------------------------

    async def start(self) -> None:
        if not self._config.enabled:
            log.info("zfs: disabled, not starting snapshot loop")
            return
        if self._task and not self._task.done():
            return
        self._task = asyncio.create_task(self._snapshot_loop(), name="zfs.snapshot_loop")
        log.info("zfs: started snapshot scheduler")

    async def stop(self) -> None:
        if self._task and not self._task.done():
            self._task.cancel()
            try:
                await self._task
            except asyncio.CancelledError:
                pass
        self._task = None
        log.info("zfs: stopped")

    # ------------------------------------------------------------------
    # Pool / dataset discovery
    # ------------------------------------------------------------------

    async def list_pools(self) -> list[dict]:
        """Return all ZFS pools visible on this system."""
        out = await self._run("zpool", "list", "-H", "-o", "name,health,size,alloc,free")
        if out is None:
            return []
        pools = []
        for line in out.strip().splitlines():
            parts = line.split("\t")
            if len(parts) >= 5:
                pools.append({
                    "name": parts[0],
                    "health": parts[1],
                    "size": parts[2],
                    "alloc": parts[3],
                    "free": parts[4],
                })
        return pools

    async def list_datasets(self, pool: str | None = None) -> list[ZFSDataset]:
        """List datasets, optionally filtered to a pool."""
        args = ["zfs", "list", "-H", "-t", "filesystem",
                "-o", "name,mountpoint,used,avail,refer,encryption,compression,quota"]
        if pool:
            args.append(pool)
        out = await self._run(*args)
        if out is None:
            return []
        datasets = []
        for line in out.strip().splitlines():
            parts = line.split("\t")
            if len(parts) < 8:
                continue
            name, mp, used, avail, refer, enc, comp, quota = parts
            datasets.append(ZFSDataset(
                name=name,
                pool=name.split("/")[0],
                mountpoint=mp,
                used_bytes=_parse_zfs_size(used),
                avail_bytes=_parse_zfs_size(avail),
                refer_bytes=_parse_zfs_size(refer),
                encrypted=enc not in ("off", "-"),
                compression=comp if comp != "-" else "off",
                quota=_parse_zfs_size(quota) if quota not in ("-", "none") else 0,
            ))
        return datasets

    async def list_snapshots(self, dataset: str | None = None) -> list[ZFSSnapshot]:
        """List snapshots for a dataset (or all managed datasets)."""
        args = ["zfs", "list", "-H", "-t", "snapshot",
                "-o", "name,creation,used", "-S", "creation"]
        if dataset:
            args.append(dataset)
        out = await self._run(*args)
        if out is None:
            return []
        snaps = []
        for line in out.strip().splitlines():
            parts = line.split("\t")
            if len(parts) < 3:
                continue
            full_name, creation_str, used = parts
            if "@" not in full_name:
                continue
            ds, short = full_name.split("@", 1)
            short = "@" + short
            snaps.append(ZFSSnapshot(
                name=full_name,
                dataset=ds,
                short_name=short,
                created=_parse_zfs_date(creation_str),
                used_bytes=_parse_zfs_size(used),
            ))
        return snaps

    # ------------------------------------------------------------------
    # Dataset management
    # ------------------------------------------------------------------

    async def create_dataset(
        self,
        dataset: str,
        mountpoint: str | None = None,
        encrypted: bool | None = None,
        quota_bytes: int = 0,
        compression: str = "lz4",
    ) -> bool:
        """Create a ZFS dataset. Returns True on success."""
        if encrypted is None:
            encrypted = self._config.default_encryption

        args = ["zfs", "create", "-p"]
        if encrypted:
            args += ["-o", "encryption=aes-256-gcm", "-o", "keylocation=prompt", "-o", "keyformat=passphrase"]
        args += ["-o", f"compression={compression}"]
        if mountpoint:
            args += ["-o", f"mountpoint={mountpoint}"]
        if quota_bytes > 0:
            args += ["-o", f"quota={quota_bytes}"]
        args.append(dataset)

        rc = await self._run_rc(*args)
        if rc == 0:
            log.info("zfs: created dataset %s (encrypted=%s)", dataset, encrypted)
            return True
        log.error("zfs: failed to create dataset %s (rc=%d)", dataset, rc)
        return False

    async def destroy_dataset(self, dataset: str, recursive: bool = False) -> bool:
        args = ["zfs", "destroy"]
        if recursive:
            args.append("-r")
        args.append(dataset)
        rc = await self._run_rc(*args)
        if rc == 0:
            log.info("zfs: destroyed dataset %s", dataset)
            return True
        log.error("zfs: failed to destroy dataset %s (rc=%d)", dataset, rc)
        return False

    # ------------------------------------------------------------------
    # Managed dataset registry
    # ------------------------------------------------------------------

    def register_dataset(self, dataset: str, **kwargs) -> ZFSManagedDataset:
        """Register a dataset for Ozma-managed snapshots + backup."""
        if dataset in self._managed:
            md = self._managed[dataset]
            for k, v in kwargs.items():
                if hasattr(md, k):
                    setattr(md, k, v)
        else:
            md = ZFSManagedDataset(dataset=dataset, **kwargs)
            self._managed[dataset] = md
        self._save()
        log.info("zfs: registered managed dataset %s", dataset)
        return md

    def unregister_dataset(self, dataset: str) -> bool:
        if dataset not in self._managed:
            return False
        del self._managed[dataset]
        self._save()
        return True

    def get_managed(self, dataset: str) -> ZFSManagedDataset | None:
        return self._managed.get(dataset)

    def list_managed(self) -> list[dict]:
        return [md.to_dict() for md in self._managed.values()]

    def update_managed(self, dataset: str, **kwargs) -> ZFSManagedDataset | None:
        md = self._managed.get(dataset)
        if not md:
            return None
        for k, v in kwargs.items():
            if k == "policy" and isinstance(v, dict):
                md.policy = SnapshotPolicy.from_dict(v)
            elif hasattr(md, k):
                setattr(md, k, v)
        self._save()
        return md

    # ------------------------------------------------------------------
    # Snapshots
    # ------------------------------------------------------------------

    async def take_snapshot(self, dataset: str, label: str | None = None) -> str | None:
        """Take a snapshot of a dataset. Returns snapshot name on success.

        If no label is given, uses GMT format for Samba shadow_copy2 compatibility.
        """
        if label is None:
            label = datetime.now(timezone.utc).strftime(_GMT_FMT)
        snapshot = f"{dataset}@{label}"
        rc = await self._run_rc("zfs", "snapshot", snapshot)
        if rc == 0:
            log.debug("zfs: snapshot %s", snapshot)
            return snapshot
        log.error("zfs: failed to snapshot %s (rc=%d)", snapshot, rc)
        return None

    async def destroy_snapshot(self, snapshot: str) -> bool:
        rc = await self._run_rc("zfs", "destroy", snapshot)
        if rc == 0:
            log.debug("zfs: destroyed snapshot %s", snapshot)
            return True
        log.error("zfs: failed to destroy snapshot %s (rc=%d)", snapshot, rc)
        return False

    async def prune_snapshots(self, dataset: str, policy: SnapshotPolicy) -> int:
        """Destroy snapshots that exceed the retention policy.

        Only prunes @GMT-... snapshots created by Ozma. Returns count destroyed.
        """
        snaps = await self.list_snapshots(dataset)
        # Only manage our own GMT-named snapshots
        ozma_snaps = [s for s in snaps if s.short_name.startswith("@GMT-")]
        # Sort oldest first
        ozma_snaps.sort(key=lambda s: s.created)

        # Bucket into frequencies
        hourly = [s for s in ozma_snaps if _snap_freq(s, ozma_snaps) == "hourly"]
        daily = [s for s in ozma_snaps if _snap_freq(s, ozma_snaps) == "daily"]
        weekly = [s for s in ozma_snaps if _snap_freq(s, ozma_snaps) == "weekly"]
        monthly = [s for s in ozma_snaps if _snap_freq(s, ozma_snaps) == "monthly"]

        to_destroy: list[ZFSSnapshot] = []
        if len(hourly) > policy.hourly:
            to_destroy.extend(hourly[: len(hourly) - policy.hourly])
        if len(daily) > policy.daily:
            to_destroy.extend(daily[: len(daily) - policy.daily])
        if len(weekly) > policy.weekly:
            to_destroy.extend(weekly[: len(weekly) - policy.weekly])
        if len(monthly) > policy.monthly:
            to_destroy.extend(monthly[: len(monthly) - policy.monthly])

        destroyed = 0
        for snap in to_destroy:
            if await self.destroy_snapshot(snap.name):
                destroyed += 1

        return destroyed

    # ------------------------------------------------------------------
    # Cloud backup via zfs send
    # ------------------------------------------------------------------

    async def send_to_connect(
        self,
        dataset: str,
        connect_url: str,
        auth_header: str,
    ) -> bool:
        """Stream a ZFS dataset to Ozma Connect via HTTP PUT.

        Uses incremental send if a previous snapshot is tracked, otherwise
        sends a full stream. ZFS native encryption means Connect receives
        opaque ciphertext — zero-knowledge by default.

        Returns True if the send succeeded and the baseline snapshot was updated.
        """
        md = self._managed.get(dataset)
        if md is None:
            log.error("zfs: send_to_connect: %s not in managed datasets", dataset)
            return False

        # Find the most recent snapshot to send
        snaps = await self.list_snapshots(dataset)
        ozma_snaps = sorted(
            [s for s in snaps if s.short_name.startswith("@GMT-")],
            key=lambda s: s.created,
        )
        if not ozma_snaps:
            log.warning("zfs: no snapshots to send for %s", dataset)
            return False

        latest = ozma_snaps[-1]

        # Build zfs send command
        send_cmd = ["zfs", "send", "-p"]  # -p preserves properties (including encryption)
        if md.last_sent_snapshot:
            # Incremental: only send new data since the last backup
            send_cmd += ["-I", md.last_sent_snapshot, latest.name]
            log.info("zfs: sending incremental %s → %s to Connect", md.last_sent_snapshot, latest.name)
        else:
            # Full send
            send_cmd += ["-R", latest.name]  # -R includes child datasets
            log.info("zfs: sending full stream %s to Connect", latest.name)

        try:
            import aiohttp as _aiohttp
        except ImportError:
            log.error("zfs: aiohttp not available for Connect upload")
            return False

        # Stream zfs send output directly to Connect HTTP PUT
        send_proc = await asyncio.create_subprocess_exec(
            *send_cmd,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )

        ds_encoded = dataset.replace("/", "_")
        snap_encoded = latest.short_name.lstrip("@").replace(":", "-")
        url = f"{connect_url.rstrip('/')}/zfs/{ds_encoded}/{snap_encoded}"

        headers = {
            "Authorization": auth_header,
            "Content-Type": "application/octet-stream",
            "X-Ozma-ZFS-Dataset": dataset,
            "X-Ozma-ZFS-Snapshot": latest.short_name,
            "X-Ozma-ZFS-Incremental-Base": md.last_sent_snapshot or "",
        }

        try:
            async with _aiohttp.ClientSession() as session:
                async with session.put(
                    url,
                    data=send_proc.stdout,
                    headers=headers,
                    timeout=_aiohttp.ClientTimeout(total=None),  # streaming, no timeout
                ) as resp:
                    if resp.status not in (200, 201, 204):
                        body = await resp.text()
                        log.error("zfs: Connect rejected stream: HTTP %d %s", resp.status, body[:200])
                        send_proc.kill()
                        return False

            await send_proc.wait()
            if send_proc.returncode != 0:
                stderr = await send_proc.stderr.read()
                log.error("zfs: zfs send failed: %s", stderr.decode())
                return False

        except Exception as exc:
            log.exception("zfs: send_to_connect exception: %s", exc)
            send_proc.kill()
            await send_proc.wait()
            return False

        # Update the baseline for next incremental
        md.last_sent_snapshot = latest.short_name
        md.last_sent_at = time.time()
        self._save()
        self._emit("zfs.backup_complete", {
            "dataset": dataset,
            "snapshot": latest.short_name,
            "incremental": bool(md.last_sent_snapshot),
        })
        log.info("zfs: backup complete for %s → %s", dataset, latest.short_name)
        return True

    # ------------------------------------------------------------------
    # Config
    # ------------------------------------------------------------------

    def get_config(self) -> ZFSConfig:
        return self._config

    def set_config(self, **kwargs) -> ZFSConfig:
        was_enabled = self._config.enabled
        for k, v in kwargs.items():
            if hasattr(self._config, k):
                setattr(self._config, k, v)
        self._save()
        if self._config.enabled != was_enabled:
            try:
                asyncio.get_running_loop()
                asyncio.create_task(self._handle_config_change(), name="zfs.config_change")
            except RuntimeError:
                pass  # no event loop — called from tests or synchronous context
        return self._config

    async def _handle_config_change(self) -> None:
        await self.stop()
        if self._config.enabled:
            await self.start()

    def get_status(self) -> dict:
        return {
            "enabled": self._config.enabled,
            "managed_datasets": len(self._managed),
            "scheduler_running": self._task is not None and not self._task.done(),
            "last_hourly": self._last_hourly,
            "last_daily": self._last_daily,
            "last_weekly": self._last_weekly,
            "last_monthly": self._last_monthly,
            "datasets": self.list_managed(),
        }

    # ------------------------------------------------------------------
    # Snapshot loop
    # ------------------------------------------------------------------

    async def _snapshot_loop(self) -> None:
        while True:
            now = time.time()

            for md in list(self._managed.values()):
                if not md.auto_snapshot:
                    continue

                # Hourly
                if (self._config.hourly_interval > 0
                        and now - self._last_hourly >= self._config.hourly_interval):
                    await self._do_snapshot(md, "hourly")

                # Daily
                if (self._config.daily_interval > 0
                        and now - self._last_daily >= self._config.daily_interval):
                    await self._do_snapshot(md, "daily")

                # Weekly
                if (self._config.weekly_interval > 0
                        and now - self._last_weekly >= self._config.weekly_interval):
                    await self._do_snapshot(md, "weekly")

                # Monthly
                if (self._config.monthly_interval > 0
                        and now - self._last_monthly >= self._config.monthly_interval):
                    await self._do_snapshot(md, "monthly")

                # Cloud backup
                if (md.cloud_backup and self._config.connect_backup_url
                        and md.last_sent_snapshot is not None):
                    pass  # triggered separately via send_to_connect

            # Advance timestamps
            now = time.time()
            if now - self._last_hourly >= self._config.hourly_interval:
                self._last_hourly = now
            if now - self._last_daily >= self._config.daily_interval:
                self._last_daily = now
            if now - self._last_weekly >= self._config.weekly_interval:
                self._last_weekly = now
            if now - self._last_monthly >= self._config.monthly_interval:
                self._last_monthly = now

            await asyncio.sleep(60)  # check every minute

    async def _do_snapshot(self, md: ZFSManagedDataset, freq: str) -> None:
        snap = await self.take_snapshot(md.dataset)
        if snap:
            destroyed = await self.prune_snapshots(md.dataset, md.policy)
            log.debug("zfs: %s snapshot %s (pruned %d)", freq, snap, destroyed)
            self._emit("zfs.snapshot_taken", {
                "dataset": md.dataset,
                "snapshot": snap,
                "frequency": freq,
                "pruned": destroyed,
            })

    # ------------------------------------------------------------------
    # Helpers
    # ------------------------------------------------------------------

    async def _run(self, *cmd: str) -> str | None:
        try:
            proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            stdout, stderr = await asyncio.wait_for(proc.communicate(), timeout=30)
            if proc.returncode != 0:
                log.debug("zfs cmd %s failed (rc=%d): %s", cmd[0], proc.returncode,
                          stderr.decode().strip())
                return None
            return stdout.decode()
        except asyncio.TimeoutError:
            log.warning("zfs cmd %s timed out", cmd[0])
            return None
        except FileNotFoundError:
            log.debug("zfs: command not found: %s", cmd[0])
            return None

    async def _run_rc(self, *cmd: str) -> int:
        try:
            proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            await asyncio.wait_for(proc.communicate(), timeout=30)
            return proc.returncode or 0
        except asyncio.TimeoutError:
            return 1
        except FileNotFoundError:
            return 127

    def _emit(self, event_type: str, data: dict) -> None:
        if self._event_queue:
            try:
                self._event_queue.put_nowait({"type": event_type, **data})
            except Exception:
                pass

    def _save(self) -> None:
        state = {
            "config": self._config.to_dict(),
            "managed": {ds: md.to_dict() for ds, md in self._managed.items()},
            "last_hourly": self._last_hourly,
            "last_daily": self._last_daily,
            "last_weekly": self._last_weekly,
            "last_monthly": self._last_monthly,
        }
        self._state_path.parent.mkdir(parents=True, exist_ok=True)
        tmp = self._state_path.with_suffix(".tmp")
        tmp.write_text(json.dumps(state, indent=2))
        os.chmod(tmp, 0o600)
        tmp.rename(self._state_path)

    def _load(self) -> None:
        if not self._state_path.exists():
            return
        try:
            state = json.loads(self._state_path.read_text())
            self._config = ZFSConfig.from_dict(state.get("config", {}))
            self._managed = {
                ds: ZFSManagedDataset.from_dict(md)
                for ds, md in state.get("managed", {}).items()
            }
            self._last_hourly = state.get("last_hourly", 0.0)
            self._last_daily = state.get("last_daily", 0.0)
            self._last_weekly = state.get("last_weekly", 0.0)
            self._last_monthly = state.get("last_monthly", 0.0)
            log.info("zfs: loaded %d managed dataset(s)", len(self._managed))
        except Exception as exc:
            log.error("zfs: failed to load state: %s", exc)


# ---------------------------------------------------------------------------
# Utilities
# ---------------------------------------------------------------------------

_SIZE_UNITS = {"K": 1024, "M": 1024**2, "G": 1024**3, "T": 1024**4, "P": 1024**5}


def _parse_zfs_size(s: str) -> int:
    """Parse ZFS human-readable size (e.g. '1.5G') to bytes."""
    s = s.strip()
    if s in ("-", "none", ""):
        return 0
    try:
        if s[-1].upper() in _SIZE_UNITS:
            return int(float(s[:-1]) * _SIZE_UNITS[s[-1].upper()])
        return int(s)
    except (ValueError, IndexError):
        return 0


def _parse_zfs_date(s: str) -> float:
    """Parse ZFS creation timestamp to unix time.

    ZFS outputs dates in locale-dependent format; try common forms.
    """
    s = s.strip()
    for fmt in (
        "%a %b %d %H:%M %Y",   # Wed Jan 15 02:00 2024
        "%Y-%m-%d %H:%M",
        "%d %b %Y %H:%M",
    ):
        try:
            return datetime.strptime(s, fmt).replace(tzinfo=timezone.utc).timestamp()
        except ValueError:
            continue
    return 0.0


def _snap_freq(snap: ZFSSnapshot, all_snaps: list[ZFSSnapshot]) -> str:
    """Classify a snapshot into hourly/daily/weekly/monthly bucket.

    Simple bucketing: the N-th snapshot from the end within each time bucket.
    We track the most granular bucket a snapshot belongs to.
    """
    now = time.time()
    age = now - snap.created
    if age < 86400 * 2:      # < 2 days old → hourly
        return "hourly"
    elif age < 86400 * 14:   # < 2 weeks → daily
        return "daily"
    elif age < 86400 * 60:   # < 2 months → weekly
        return "weekly"
    else:
        return "monthly"
