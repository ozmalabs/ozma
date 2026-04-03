# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Samba + NFS file sharing manager for the Ozma home system controller.
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import re
import stat
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.file_sharing")


# ---------------------------------------------------------------------------
# Data models
# ---------------------------------------------------------------------------


@dataclass
class FileShare:
    id: str
    name: str
    path: str
    protocols: list[str]
    read_only: bool = False
    guest_ok: bool = False
    valid_users: list[str] = field(default_factory=list)
    comment: str = ""
    browseable: bool = True
    create_mask: str = "0664"
    directory_mask: str = "0775"
    # ZFS dataset backing this share (e.g. "tank/shares/homes").
    # When set, Samba enables shadow_copy2 so Windows sees snapshots
    # as "Previous Versions" with zero extra client configuration.
    zfs_dataset: str | None = None

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "name": self.name,
            "path": self.path,
            "protocols": self.protocols,
            "read_only": self.read_only,
            "guest_ok": self.guest_ok,
            "valid_users": self.valid_users,
            "comment": self.comment,
            "browseable": self.browseable,
            "create_mask": self.create_mask,
            "directory_mask": self.directory_mask,
            "zfs_dataset": self.zfs_dataset,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> FileShare:
        return cls(
            id=d["id"],
            name=d["name"],
            path=d["path"],
            protocols=d["protocols"],
            read_only=d.get("read_only", False),
            guest_ok=d.get("guest_ok", False),
            valid_users=d.get("valid_users", []),
            comment=d.get("comment", ""),
            browseable=d.get("browseable", True),
            create_mask=d.get("create_mask", "0664"),
            directory_mask=d.get("directory_mask", "0775"),
            zfs_dataset=d.get("zfs_dataset"),
        )


@dataclass
class SambaUser:
    username: str
    enabled: bool = True

    def to_dict(self) -> dict[str, Any]:
        return {"username": self.username, "enabled": self.enabled}

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> SambaUser:
        return cls(username=d["username"], enabled=d.get("enabled", True))


@dataclass
class FileSharingConfig:
    enabled: bool = False
    workgroup: str = "WORKGROUP"
    server_string: str = "Ozma File Server"
    netbios_name: str = "OZMA"
    smb_enabled: bool = True
    nfs_enabled: bool = False
    min_protocol: str = "SMB2"

    def to_dict(self) -> dict[str, Any]:
        return {
            "enabled": self.enabled,
            "workgroup": self.workgroup,
            "server_string": self.server_string,
            "netbios_name": self.netbios_name,
            "smb_enabled": self.smb_enabled,
            "nfs_enabled": self.nfs_enabled,
            "min_protocol": self.min_protocol,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> FileSharingConfig:
        return cls(
            enabled=d.get("enabled", False),
            workgroup=d.get("workgroup", "WORKGROUP"),
            server_string=d.get("server_string", "Ozma File Server"),
            netbios_name=d.get("netbios_name", "OZMA"),
            smb_enabled=d.get("smb_enabled", True),
            nfs_enabled=d.get("nfs_enabled", False),
            min_protocol=d.get("min_protocol", "SMB2"),
        )


# ---------------------------------------------------------------------------
# Manager
# ---------------------------------------------------------------------------


def _share_id_from_name(name: str) -> str:
    return re.sub(r"[^a-z0-9_]", "_", name.lower())[:32]


class FileSharingManager:
    STATE_PATH = Path("/var/lib/ozma/file_sharing_state.json")
    SAMBA_CONF_PATH = Path("/tmp/ozma-smb.conf")
    NFS_EXPORTS_PATH = Path("/etc/exports.d/ozma.exports")

    def __init__(self, state_path: Path | None = None) -> None:
        if state_path is not None:
            self.STATE_PATH = state_path

        self._config = FileSharingConfig()
        self._shares: dict[str, FileShare] = {}
        self._samba_users: dict[str, SambaUser] = {}

        self._smbd_proc: asyncio.subprocess.Process | None = None
        self._nmbd_proc: asyncio.subprocess.Process | None = None
        self._active = False

    # ------------------------------------------------------------------
    # Lifecycle
    # ------------------------------------------------------------------

    async def start(self) -> None:
        self._load()
        if self._config.enabled:
            await self._apply()
        log.info("file_sharing: manager started (enabled=%s)", self._config.enabled)

    async def stop(self) -> None:
        await self._teardown()
        log.info("file_sharing: manager stopped")

    # ------------------------------------------------------------------
    # Share CRUD
    # ------------------------------------------------------------------

    def add_share(
        self,
        name: str,
        path: str,
        protocols: list[str] | None = None,
        **kwargs: Any,
    ) -> FileShare:
        if protocols is None:
            protocols = ["smb"]

        share_id = _share_id_from_name(name)
        share = FileShare(
            id=share_id,
            name=name,
            path=path,
            protocols=protocols,
            read_only=kwargs.get("read_only", False),
            guest_ok=kwargs.get("guest_ok", False),
            valid_users=kwargs.get("valid_users", []),
            comment=kwargs.get("comment", ""),
            browseable=kwargs.get("browseable", True),
            create_mask=kwargs.get("create_mask", "0664"),
            directory_mask=kwargs.get("directory_mask", "0775"),
        )
        self._shares[share_id] = share
        self._save()
        log.info("file_sharing: added share %r (%s) protocols=%s", name, path, protocols)
        return share

    def update_share(self, share_id: str, **kwargs: Any) -> FileShare | None:
        share = self._shares.get(share_id)
        if share is None:
            return None

        for key, value in kwargs.items():
            if hasattr(share, key):
                setattr(share, key, value)
            else:
                log.warning("file_sharing: unknown share field %r — ignored", key)

        self._save()
        log.info("file_sharing: updated share %r", share_id)
        return share

    def remove_share(self, share_id: str) -> bool:
        if share_id not in self._shares:
            return False
        del self._shares[share_id]
        self._save()
        log.info("file_sharing: removed share %r", share_id)
        return True

    def list_shares(self) -> list[dict[str, Any]]:
        return [s.to_dict() for s in self._shares.values()]

    def get_share(self, share_id: str) -> FileShare | None:
        return self._shares.get(share_id)

    # ------------------------------------------------------------------
    # Samba user management
    # ------------------------------------------------------------------

    async def add_samba_user(self, username: str, password: str) -> bool:
        input_bytes = f"{password}\n{password}\n".encode()
        try:
            proc = await asyncio.create_subprocess_exec(
                "smbpasswd", "-s", "-a", username,
                stdin=asyncio.subprocess.PIPE,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            stdout, stderr = await proc.communicate(input=input_bytes)
            if proc.returncode != 0:
                log.error(
                    "file_sharing: smbpasswd -a failed for %r (rc=%d): %s",
                    username,
                    proc.returncode,
                    stderr.decode(errors="replace").strip(),
                )
                return False
        except FileNotFoundError:
            log.error("file_sharing: smbpasswd not found — is samba installed?")
            return False

        self._samba_users[username] = SambaUser(username=username, enabled=True)
        self._save()
        log.info("file_sharing: samba user %r added", username)
        return True

    async def remove_samba_user(self, username: str) -> bool:
        try:
            proc = await asyncio.create_subprocess_exec(
                "smbpasswd", "-x", username,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            stdout, stderr = await proc.communicate()
            if proc.returncode != 0:
                log.error(
                    "file_sharing: smbpasswd -x failed for %r (rc=%d): %s",
                    username,
                    proc.returncode,
                    stderr.decode(errors="replace").strip(),
                )
                return False
        except FileNotFoundError:
            log.error("file_sharing: smbpasswd not found — is samba installed?")
            return False

        self._samba_users.pop(username, None)
        self._save()
        log.info("file_sharing: samba user %r removed", username)
        return True

    def list_samba_users(self) -> list[dict[str, Any]]:
        return [u.to_dict() for u in self._samba_users.values()]

    # ------------------------------------------------------------------
    # Config
    # ------------------------------------------------------------------

    def get_config(self) -> FileSharingConfig:
        return self._config

    async def set_config(self, **kwargs: Any) -> FileSharingConfig:
        was_enabled = self._config.enabled

        for key, value in kwargs.items():
            if hasattr(self._config, key):
                setattr(self._config, key, value)
            else:
                log.warning("file_sharing: unknown config field %r — ignored", key)

        self._save()

        if self._config.enabled and not was_enabled:
            await self._apply()
        elif not self._config.enabled and was_enabled:
            await self._teardown()
        elif self._config.enabled:
            # Re-apply with new settings
            await self._apply()

        log.info("file_sharing: config updated enabled=%s", self._config.enabled)
        return self._config

    def get_status(self) -> dict[str, Any]:
        smbd_running = (
            self._smbd_proc is not None
            and self._smbd_proc.returncode is None
        )
        # nfsd is managed via exportfs; track whether we have active exports
        nfsd_running = self._active and self._config.nfs_enabled

        return {
            "enabled": self._config.enabled,
            "active": self._active,
            "share_count": len(self._shares),
            "smb_enabled": self._config.smb_enabled,
            "nfs_enabled": self._config.nfs_enabled,
            "smbd_running": smbd_running,
            "nfsd_running": nfsd_running,
        }

    # ------------------------------------------------------------------
    # Apply / teardown
    # ------------------------------------------------------------------

    async def _apply(self) -> None:
        if self._config.smb_enabled:
            conf = self._build_smb_conf()
            self.SAMBA_CONF_PATH.write_text(conf)
            log.debug("file_sharing: wrote smb.conf to %s", self.SAMBA_CONF_PATH)
            if self._smbd_proc is not None and self._smbd_proc.returncode is None:
                await self._reload_smbd()
            else:
                await self._start_smbd()

        if self._config.nfs_enabled:
            await self._apply_nfs()

        self._active = True

    async def _teardown(self) -> None:
        if self._config.smb_enabled or self._smbd_proc is not None:
            await self._stop_smbd()

        if self._config.nfs_enabled:
            await self._stop_nfs()

        self._active = False

    # ------------------------------------------------------------------
    # Samba: config generation
    # ------------------------------------------------------------------

    def _build_smb_conf(self) -> str:
        cfg = self._config
        lines: list[str] = [
            "[global]",
            f"   workgroup = {cfg.workgroup}",
            f"   server string = {cfg.server_string}",
            f"   netbios name = {cfg.netbios_name}",
            f"   server min protocol = {cfg.min_protocol}",
            "   security = user",
            "   map to guest = Bad User",
            "   log level = 1",
            "   # Performance",
            "   socket options = TCP_NODELAY IPTOS_LOWDELAY SO_RCVBUF=131072 SO_SNDBUF=131072",
            "   read raw = yes",
            "   write raw = yes",
            "   oplocks = yes",
            "   max xmit = 65535",
            "   dead time = 15",
            "   getwd cache = yes",
        ]

        smb_shares = [
            s for s in self._shares.values()
            if any(p in ("smb", "both") for p in s.protocols)
        ]

        for share in smb_shares:
            lines.append("")
            lines.append(f"[{share.name}]")
            if share.comment:
                lines.append(f"   comment = {share.comment}")
            lines.append(f"   path = {share.path}")
            lines.append(f"   browseable = {'yes' if share.browseable else 'no'}")
            lines.append(f"   read only = {'yes' if share.read_only else 'no'}")
            lines.append(f"   guest ok = {'yes' if share.guest_ok else 'no'}")
            if share.valid_users:
                lines.append(f"   valid users = {' '.join(share.valid_users)}")
            lines.append(f"   create mask = {share.create_mask}")
            lines.append(f"   directory mask = {share.directory_mask}")
            # ZFS-backed share: enable shadow_copy2 so Windows "Previous Versions"
            # tab shows ZFS snapshots automatically. Snapshots must be named with
            # the @GMT-YYYY.MM.DD-HH.MM.SS format (which ZFSManager uses).
            if share.zfs_dataset:
                lines.append("   vfs objects = shadow_copy2")
                lines.append("   shadow:snapdir = .zfs/snapshot")
                lines.append("   shadow:sort = desc")
                # ZFSManager names snapshots GMT-YYYY.MM.DD-HH.MM.SS (no leading @)
                # so they appear in .zfs/snapshot/ without @ prefix.
                lines.append("   shadow:format = GMT-%Y.%m.%d-%H.%M.%S")
                lines.append("   shadow:localtime = no")

        return "\n".join(lines) + "\n"

    # ------------------------------------------------------------------
    # Samba: process management
    # ------------------------------------------------------------------

    async def _start_smbd(self) -> None:
        log.info("file_sharing: starting smbd with configfile=%s", self.SAMBA_CONF_PATH)
        try:
            self._smbd_proc = await asyncio.create_subprocess_exec(
                "smbd",
                "--foreground",
                "--no-process-group",
                f"--configfile={self.SAMBA_CONF_PATH}",
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            asyncio.create_task(
                self._drain_proc(self._smbd_proc, "smbd"),
                name="file_sharing.smbd_drain",
            )
        except FileNotFoundError:
            log.error("file_sharing: smbd not found — is samba installed?")
            return

        try:
            self._nmbd_proc = await asyncio.create_subprocess_exec(
                "nmbd",
                "--foreground",
                "--no-process-group",
                f"--configfile={self.SAMBA_CONF_PATH}",
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            asyncio.create_task(
                self._drain_proc(self._nmbd_proc, "nmbd"),
                name="file_sharing.nmbd_drain",
            )
        except FileNotFoundError:
            log.warning("file_sharing: nmbd not found — NetBIOS name resolution unavailable")

    async def _stop_smbd(self) -> None:
        for proc, name in ((self._smbd_proc, "smbd"), (self._nmbd_proc, "nmbd")):
            if proc is not None and proc.returncode is None:
                log.info("file_sharing: stopping %s (pid=%d)", name, proc.pid)
                proc.terminate()
                try:
                    await asyncio.wait_for(proc.wait(), timeout=10.0)
                except asyncio.TimeoutError:
                    log.warning("file_sharing: %s did not exit — sending SIGKILL", name)
                    proc.kill()
                    await proc.wait()

        self._smbd_proc = None
        self._nmbd_proc = None

    async def _reload_smbd(self) -> None:
        log.info("file_sharing: reloading smbd config")
        try:
            proc = await asyncio.create_subprocess_exec(
                "smbcontrol", "smbd", "reload-config",
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            stdout, stderr = await proc.communicate()
            if proc.returncode != 0:
                log.warning(
                    "file_sharing: smbcontrol reload-config failed (rc=%d): %s",
                    proc.returncode,
                    stderr.decode(errors="replace").strip(),
                )
        except FileNotFoundError:
            log.warning("file_sharing: smbcontrol not found — restart smbd instead")
            await self._stop_smbd()
            await self._start_smbd()

    # ------------------------------------------------------------------
    # NFS: config generation
    # ------------------------------------------------------------------

    def _build_nfs_exports(self) -> str:
        lines: list[str] = [
            "# Managed by Ozma — do not edit manually",
        ]
        nfs_shares = [
            s for s in self._shares.values()
            if any(p in ("nfs", "both") for p in s.protocols)
        ]
        for share in nfs_shares:
            if share.read_only:
                options = "ro,sync,no_subtree_check"
            else:
                options = "rw,sync,no_subtree_check,no_root_squash"
            lines.append(f"{share.path} *({options})")

        return "\n".join(lines) + "\n"

    # ------------------------------------------------------------------
    # NFS: process management
    # ------------------------------------------------------------------

    async def _apply_nfs(self) -> None:
        exports_dir = self.NFS_EXPORTS_PATH.parent
        if not exports_dir.exists():
            try:
                exports_dir.mkdir(parents=True, exist_ok=True)
            except PermissionError:
                log.warning(
                    "file_sharing: cannot create NFS exports dir %s — skipping NFS",
                    exports_dir,
                )
                return

        content = self._build_nfs_exports()
        try:
            self.NFS_EXPORTS_PATH.write_text(content)
            log.debug("file_sharing: wrote NFS exports to %s", self.NFS_EXPORTS_PATH)
        except PermissionError:
            log.warning(
                "file_sharing: cannot write NFS exports to %s — skipping NFS",
                self.NFS_EXPORTS_PATH,
            )
            return

        try:
            proc = await asyncio.create_subprocess_exec(
                "exportfs", "-ra",
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            stdout, stderr = await proc.communicate()
            if proc.returncode != 0:
                log.error(
                    "file_sharing: exportfs -ra failed (rc=%d): %s",
                    proc.returncode,
                    stderr.decode(errors="replace").strip(),
                )
            else:
                log.info("file_sharing: NFS exports applied")
        except FileNotFoundError:
            log.error("file_sharing: exportfs not found — is nfs-kernel-server installed?")

    async def _stop_nfs(self) -> None:
        if not self.NFS_EXPORTS_PATH.exists():
            return

        log.info("file_sharing: unexporting Ozma NFS shares")
        try:
            proc = await asyncio.create_subprocess_exec(
                "exportfs", "-ua",
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            stdout, stderr = await proc.communicate()
            if proc.returncode != 0:
                log.warning(
                    "file_sharing: exportfs -ua failed (rc=%d): %s",
                    proc.returncode,
                    stderr.decode(errors="replace").strip(),
                )
        except FileNotFoundError:
            log.warning("file_sharing: exportfs not found — NFS teardown skipped")

        try:
            self.NFS_EXPORTS_PATH.unlink(missing_ok=True)
        except PermissionError:
            log.warning(
                "file_sharing: cannot remove %s — manual cleanup may be needed",
                self.NFS_EXPORTS_PATH,
            )

    # ------------------------------------------------------------------
    # Persistence
    # ------------------------------------------------------------------

    def _save(self) -> None:
        state = {
            "config": self._config.to_dict(),
            "shares": {sid: s.to_dict() for sid, s in self._shares.items()},
            "samba_users": {u: su.to_dict() for u, su in self._samba_users.items()},
        }
        data = json.dumps(state, indent=2)

        self.STATE_PATH.parent.mkdir(parents=True, exist_ok=True)
        tmp = self.STATE_PATH.with_suffix(".tmp")
        tmp.write_text(data)
        tmp.chmod(stat.S_IRUSR | stat.S_IWUSR)  # 0o600
        tmp.rename(self.STATE_PATH)

        log.debug("file_sharing: state saved to %s", self.STATE_PATH)

    def _load(self) -> None:
        if not self.STATE_PATH.exists():
            log.debug("file_sharing: no state file at %s — using defaults", self.STATE_PATH)
            return

        try:
            state = json.loads(self.STATE_PATH.read_text())
        except (json.JSONDecodeError, OSError) as exc:
            log.error("file_sharing: failed to load state from %s: %s", self.STATE_PATH, exc)
            return

        if "config" in state:
            self._config = FileSharingConfig.from_dict(state["config"])

        self._shares = {
            sid: FileShare.from_dict(d)
            for sid, d in state.get("shares", {}).items()
        }

        self._samba_users = {
            u: SambaUser.from_dict(d)
            for u, d in state.get("samba_users", {}).items()
        }

        log.info(
            "file_sharing: loaded %d share(s), %d samba user(s)",
            len(self._shares),
            len(self._samba_users),
        )

    # ------------------------------------------------------------------
    # Internal helpers
    # ------------------------------------------------------------------

    @staticmethod
    async def _drain_proc(proc: asyncio.subprocess.Process, label: str) -> None:
        """Drain stdout/stderr of a subprocess to the logger until it exits."""
        async def _drain_stream(stream: asyncio.StreamReader | None, level: int) -> None:
            if stream is None:
                return
            while True:
                line = await stream.readline()
                if not line:
                    break
                log.log(level, "file_sharing[%s]: %s", label, line.decode(errors="replace").rstrip())

        await asyncio.gather(
            _drain_stream(proc.stdout, logging.DEBUG),
            _drain_stream(proc.stderr, logging.WARNING),
        )
        rc = await proc.wait()
        log.info("file_sharing: %s exited with rc=%d", label, rc)
