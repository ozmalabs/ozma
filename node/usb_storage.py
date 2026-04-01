# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
USB mass storage subsystem — bridge any storage to any target via USB.

The hardware node presents a USB mass storage device (a "USB drive") to
the target machine. The contents of that drive can be backed by anything:

  Local:      A FAT32 image on the node's filesystem
  rclone:     ANY of rclone's 70+ backends mounted as the storage:
              S3, Google Drive, OneDrive, Dropbox, SFTP, SMB, FTP,
              Azure Blob, Backblaze B2, Nextcloud, WebDAV, Mega, etc.
  ISO:        A bootable ISO image (for OS installation)
  Controller: Files pushed from the dashboard

The target machine sees a standard USB drive. It has no network. It
doesn't need network. The node provides network via the USB cable —
the "USB drive" IS the network bridge.

Use cases:
  1. Network driver delivery: new server has no network drivers. The node's
     USB drive contains the drivers (fetched from S3 or a share). Install
     drivers from the "USB stick". Server has network. Zero physical media.

  2. Air-gapped software delivery: soft node installer on the USB drive.
     Target runs the installer from the "USB stick". No network on target.

  3. Cloud storage without network: target saves files to the "USB drive",
     they sync to Google Drive / S3 / OneDrive via rclone on the node.
     The target machine has never been on a network. Its files are in the cloud.

  4. OS installation: mount an ISO as the USB drive. Target boots from it.

  5. File transfer: drag files from the ozma dashboard → node → target USB.

  6. Evidence capture: target writes to the "USB drive". Files captured by
     the node and forwarded to S3/evidence bucket.

  7. Provisioning: automation scripts, config files, packages — all on the
     USB drive, sourced from any backend.

Architecture:
  ┌──────────────┐         ┌───────────────┐         ┌──────────────┐
  │ Cloud/Network │◄─rclone─│  Ozma Node    │──USB──►│ Target Machine│
  │ (S3, GDrive,  │         │  mass_storage │         │ (sees USB    │
  │  SMB, SFTP..) │         │  gadget       │         │  drive)      │
  └──────────────┘         └───────────────┘         └──────────────┘

rclone runs on the node. It mounts the remote backend as a local
directory (FUSE mount). That directory is the contents of the FAT32
image (or is served directly via rclone's VFS mount). The USB gadget
presents the image to the target. Changes sync bidirectionally.
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import shutil
import subprocess
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.node.usb_storage")

GADGET_DIR = Path("/sys/kernel/config/usb_gadget/ozma")
STORAGE_DIR = Path("/opt/ozma/storage")
DEFAULT_IMAGE_SIZE_MB = 256
RCLONE_CONFIG_DIR = Path("/opt/ozma/storage/rclone")


@dataclass
class StorageBackend:
    """A storage backend configuration."""
    id: str
    backend_type: str       # local, rclone, iso, controller
    path: str = ""          # local path, ISO path, or rclone remote:path
    rclone_remote: str = "" # rclone remote name (e.g., "s3", "gdrive")
    rclone_path: str = ""   # path within the remote
    read_only: bool = False
    active: bool = False

    def to_dict(self) -> dict:
        return {
            "id": self.id, "type": self.backend_type,
            "path": self.path, "rclone_remote": self.rclone_remote,
            "read_only": self.read_only, "active": self.active,
        }


class USBStorageManager:
    """
    Manages USB mass storage with pluggable backends via rclone.

    The storage appears as a USB drive to the target. The contents
    come from any rclone-supported backend, a local directory, an
    ISO image, or files pushed from the controller.
    """

    def __init__(self, storage_dir: Path = STORAGE_DIR,
                 image_size_mb: int = DEFAULT_IMAGE_SIZE_MB) -> None:
        self._storage_dir = storage_dir
        self._image_size_mb = image_size_mb
        self._image_path = storage_dir / "ozma-storage.img"
        self._mount_path = storage_dir / "mount"
        self._rclone_mount_path = storage_dir / "rclone-mount"
        self._active = False
        self._backend: StorageBackend | None = None
        self._rclone_proc: asyncio.subprocess.Process | None = None
        self._sync_task: asyncio.Task | None = None
        self._backends: dict[str, StorageBackend] = {}
        self._load_config()

    def _load_config(self) -> None:
        config_path = self._storage_dir / "storage_config.json"
        if config_path.exists():
            try:
                data = json.loads(config_path.read_text())
                for b in data.get("backends", []):
                    backend = StorageBackend(**{k: v for k, v in b.items()
                                               if k in StorageBackend.__dataclass_fields__})
                    self._backends[backend.id] = backend
            except Exception:
                pass

    def _save_config(self) -> None:
        self._storage_dir.mkdir(parents=True, exist_ok=True)
        config_path = self._storage_dir / "storage_config.json"
        data = {"backends": [b.to_dict() for b in self._backends.values()]}
        config_path.write_text(json.dumps(data, indent=2))

    @property
    def active(self) -> bool:
        return self._active

    @property
    def mount_path(self) -> Path:
        return self._mount_path

    # ── Lifecycle ───────────────────────────────────────────────────────────

    async def start(self, backend_id: str = "local") -> bool:
        """Start USB storage with the specified backend."""
        self._storage_dir.mkdir(parents=True, exist_ok=True)
        self._mount_path.mkdir(parents=True, exist_ok=True)

        # Create default local backend if none configured
        if "local" not in self._backends:
            self._backends["local"] = StorageBackend(
                id="local", backend_type="local",
                path=str(self._mount_path),
            )

        # Create the FAT32 image if it doesn't exist
        if not self._image_path.exists():
            if not await self._create_image():
                return False

        # Mount the image locally
        await self._mount_image()

        # Set up the requested backend
        backend = self._backends.get(backend_id)
        if backend and backend.backend_type == "rclone":
            await self._start_rclone_sync(backend)
        elif backend and backend.backend_type == "iso":
            await self._mount_iso(backend.path)
            self._backend = backend
            self._active = True
            return True

        # Add gadget function
        await self._add_gadget_function()

        self._backend = backend
        self._active = True
        log.info("USB storage active: backend=%s", backend_id)
        return True

    async def stop(self) -> None:
        await self._stop_rclone()
        await self._unmount_image()
        self._active = False

    # ── Backend management ──────────────────────────────────────────────────

    def add_backend(self, backend: StorageBackend) -> None:
        """Register a storage backend."""
        self._backends[backend.id] = backend
        self._save_config()

    def remove_backend(self, backend_id: str) -> bool:
        if backend_id in self._backends and backend_id != "local":
            del self._backends[backend_id]
            self._save_config()
            return True
        return False

    def list_backends(self) -> list[dict]:
        return [b.to_dict() for b in self._backends.values()]

    async def switch_backend(self, backend_id: str) -> bool:
        """Switch to a different storage backend."""
        if backend_id not in self._backends:
            return False
        await self.stop()
        return await self.start(backend_id)

    # ── rclone integration ──────────────────────────────────────────────────

    def configure_rclone_remote(self, remote_name: str, remote_type: str,
                                 config: dict) -> bool:
        """
        Configure an rclone remote.

        Examples:
          configure_rclone_remote("s3", "s3", {"provider": "AWS", "access_key_id": "...", ...})
          configure_rclone_remote("gdrive", "drive", {"client_id": "...", ...})
          configure_rclone_remote("office-share", "smb", {"host": "10.0.0.5", "user": "..."})
        """
        if not shutil.which("rclone"):
            log.warning("rclone not installed — remote storage unavailable")
            return False

        RCLONE_CONFIG_DIR.mkdir(parents=True, exist_ok=True)
        rclone_conf = RCLONE_CONFIG_DIR / "rclone.conf"

        # Append the remote config
        section = f"\n[{remote_name}]\ntype = {remote_type}\n"
        for k, v in config.items():
            section += f"{k} = {v}\n"

        with open(rclone_conf, "a") as f:
            f.write(section)

        # Register as a backend
        self.add_backend(StorageBackend(
            id=f"rclone-{remote_name}",
            backend_type="rclone",
            rclone_remote=remote_name,
            rclone_path="/",
        ))

        log.info("rclone remote configured: %s (type=%s)", remote_name, remote_type)
        return True

    async def _start_rclone_sync(self, backend: StorageBackend) -> None:
        """Start rclone sync/mount for a remote backend."""
        if not shutil.which("rclone"):
            log.warning("rclone not installed")
            return

        remote_path = f"{backend.rclone_remote}:{backend.rclone_path}"
        rclone_conf = RCLONE_CONFIG_DIR / "rclone.conf"

        # Option 1: rclone sync (periodic copy — simpler, works with FAT32 image)
        # Option 2: rclone mount (FUSE — real-time, but can't use FAT32 image)
        # We use sync: copy remote → local mount, then periodically sync back

        # Initial sync: remote → local
        log.info("Syncing from %s → USB storage...", remote_path)
        try:
            proc = await asyncio.create_subprocess_exec(
                "rclone", "sync", remote_path, str(self._mount_path),
                "--config", str(rclone_conf),
                "--transfers", "4",
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE,
            )
            _, stderr = await asyncio.wait_for(proc.communicate(), timeout=300)
            if proc.returncode == 0:
                log.info("Initial sync complete: %s → USB storage", remote_path)
                subprocess.run(["sync"], capture_output=True)  # flush to disk image
            else:
                log.warning("rclone sync failed: %s", stderr.decode()[:200])
        except asyncio.TimeoutError:
            log.warning("rclone sync timed out")
        except Exception as e:
            log.warning("rclone sync error: %s", e)

        # Start periodic bidirectional sync
        self._sync_task = asyncio.create_task(
            self._sync_loop(backend), name="rclone-sync"
        )

    async def _sync_loop(self, backend: StorageBackend) -> None:
        """Periodically sync between local mount and remote."""
        remote_path = f"{backend.rclone_remote}:{backend.rclone_path}"
        rclone_conf = RCLONE_CONFIG_DIR / "rclone.conf"

        while True:
            await asyncio.sleep(30)  # sync every 30 seconds

            # Bidirectional: local changes → remote, remote changes → local
            try:
                # Local → remote (files the target wrote to the USB drive)
                if not backend.read_only:
                    await asyncio.create_subprocess_exec(
                        "rclone", "sync", str(self._mount_path), remote_path,
                        "--config", str(rclone_conf),
                        "--transfers", "2",
                        stdout=asyncio.subprocess.DEVNULL,
                        stderr=asyncio.subprocess.DEVNULL,
                    )

                # Remote → local (new files pushed via dashboard/cloud)
                await asyncio.create_subprocess_exec(
                    "rclone", "sync", remote_path, str(self._mount_path),
                    "--config", str(rclone_conf),
                    "--transfers", "2",
                    stdout=asyncio.subprocess.DEVNULL,
                    stderr=asyncio.subprocess.DEVNULL,
                )
                subprocess.run(["sync"], capture_output=True)
            except Exception:
                pass

    async def _stop_rclone(self) -> None:
        if self._sync_task:
            self._sync_task.cancel()
            self._sync_task = None
        if self._rclone_proc and self._rclone_proc.returncode is None:
            self._rclone_proc.terminate()
            self._rclone_proc = None

    # ── Image management ────────────────────────────────────────────────────

    async def _create_image(self) -> bool:
        try:
            loop = asyncio.get_running_loop()
            def _create():
                subprocess.run(
                    ["dd", "if=/dev/zero", f"of={self._image_path}",
                     "bs=1M", f"count={self._image_size_mb}"],
                    capture_output=True, check=True,
                )
                subprocess.run(
                    ["mkfs.vfat", "-F", "32", "-n", "OZMA", str(self._image_path)],
                    capture_output=True, check=True,
                )
            await loop.run_in_executor(None, _create)
            log.info("Created storage image: %dMB FAT32", self._image_size_mb)
            return True
        except Exception as e:
            log.error("Failed to create storage image: %s", e)
            return False

    async def _mount_image(self) -> None:
        try:
            subprocess.run(
                ["mount", "-o", "loop", str(self._image_path), str(self._mount_path)],
                capture_output=True, check=True,
            )
        except Exception:
            pass

    async def _unmount_image(self) -> None:
        try:
            subprocess.run(["umount", str(self._mount_path)], capture_output=True)
        except Exception:
            pass

    async def _add_gadget_function(self) -> bool:
        func_dir = GADGET_DIR / "functions" / "mass_storage.usb0"
        if not GADGET_DIR.exists():
            return False
        try:
            func_dir.mkdir(parents=True, exist_ok=True)
            lun_dir = func_dir / "lun.0"
            lun_dir.mkdir(exist_ok=True)
            (lun_dir / "file").write_text(str(self._image_path))
            (lun_dir / "removable").write_text("1")
            (lun_dir / "ro").write_text("1" if (self._backend and self._backend.read_only) else "0")
            (lun_dir / "cdrom").write_text("0")
            config_dir = GADGET_DIR / "configs" / "c.1"
            link = config_dir / "mass_storage.usb0"
            if not link.exists():
                link.symlink_to(func_dir)
            return True
        except Exception:
            return False

    # ── ISO mount ───────────────────────────────────────────────────────────

    async def _mount_iso(self, iso_path: str) -> bool:
        func_dir = GADGET_DIR / "functions" / "mass_storage.usb0" / "lun.0"
        if not func_dir.exists():
            await self._add_gadget_function()
            func_dir = GADGET_DIR / "functions" / "mass_storage.usb0" / "lun.0"
        try:
            (func_dir / "file").write_text(iso_path)
            (func_dir / "cdrom").write_text("1")
            (func_dir / "ro").write_text("1")
            log.info("ISO mounted: %s", iso_path)
            return True
        except Exception as e:
            log.error("Failed to mount ISO: %s", e)
            return False

    # ── File operations ─────────────────────────────────────────────────────

    async def add_file(self, filename: str, content: bytes) -> bool:
        try:
            target = self._mount_path / filename
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(content)
            subprocess.run(["sync"], capture_output=True)
            return True
        except Exception as e:
            log.error("Failed to add file %s: %s", filename, e)
            return False

    async def add_file_from_path(self, src: str, dest_name: str = "") -> bool:
        src_path = Path(src)
        if not src_path.exists():
            return False
        dest = self._mount_path / (dest_name or src_path.name)
        try:
            shutil.copy2(str(src_path), str(dest))
            subprocess.run(["sync"], capture_output=True)
            return True
        except Exception:
            return False

    async def remove_file(self, filename: str) -> bool:
        try:
            (self._mount_path / filename).unlink()
            return True
        except Exception:
            return False

    async def read_file(self, filename: str) -> bytes | None:
        try:
            return (self._mount_path / filename).read_bytes()
        except Exception:
            return None

    def list_files(self) -> list[dict]:
        files = []
        if self._mount_path.exists():
            for f in sorted(self._mount_path.rglob("*")):
                if f.is_file():
                    rel = f.relative_to(self._mount_path)
                    files.append({
                        "name": str(rel),
                        "size": f.stat().st_size,
                        "modified": f.stat().st_mtime,
                    })
        return files

    # ── Seed installers ─────────────────────────────────────────────────────

    async def seed_softnode_installer(self) -> bool:
        """Pre-populate with ozma-softnode for air-gapped delivery."""
        import glob as g
        wheels = g.glob("/opt/ozma/pip-cache/ozma_softnode-*.whl") + \
                 g.glob("/opt/ozma/softnode/dist/ozma_softnode-*.whl")
        for whl in wheels:
            await self.add_file_from_path(whl)

        await self.add_file("README.txt", (
            "Ozma Soft Node Installer\n"
            "========================\n\n"
            "Install:\n"
            "  pip install ozma_softnode-*.whl\n"
            "  ozma-softnode --name $(hostname)\n"
        ).encode())

        await self.add_file("install.sh", (
            "#!/bin/bash\n"
            "cd \"$(dirname \"$0\")\"\n"
            "pip3 install ozma_softnode-*.whl && "
            "echo 'Run: ozma-softnode --name $(hostname)'\n"
        ).encode())
        return True

    async def seed_network_drivers(self, driver_path: str) -> bool:
        """Pre-populate with network drivers for the target machine."""
        src = Path(driver_path)
        if src.is_dir():
            for f in src.rglob("*"):
                if f.is_file():
                    rel = f.relative_to(src)
                    await self.add_file(f"drivers/{rel}", f.read_bytes())
        elif src.is_file():
            await self.add_file_from_path(str(src), f"drivers/{src.name}")
        return True

    # ── Status ──────────────────────────────────────────────────────────────

    def status(self) -> dict:
        return {
            "active": self._active,
            "backend": self._backend.to_dict() if self._backend else None,
            "image_size_mb": self._image_size_mb,
            "files": self.list_files() if self._active else [],
            "available_backends": self.list_backends(),
            "rclone_available": shutil.which("rclone") is not None,
        }
