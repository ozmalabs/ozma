# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Virtual media — synthesise a FAT32 block device from any file tree.

No image file. No mounting. No root. The FAT32 structures are computed
on the fly when the consumer reads sectors. The file tree can be:
  - A local directory
  - An rclone mount (FUSE — any of 70+ cloud backends)
  - Files pushed from the controller dashboard

The synthesised block device is served via:
  - NBD server → QEMU/hypervisors connect as a block device
  - Direct file backing → hardware node USB gadget reads sectors

This is the same code for both soft nodes and hardware nodes. The only
difference is the consumer: NBD for VMs, USB gadget for physical machines.

Architecture:
  rclone mount / local dir / Connect storage
          │
          ▼
    FATSynthesiser (Python, in-memory FAT32 tables)
          │
          ├── NBDServer → QEMU -drive nbd://localhost:10809
          └── read_sectors(offset, length) → USB gadget backing

The FAT32 structures (boot sector, FAT tables, root directory, data
region) are computed lazily from the file tree. Directory entries and
cluster chains are built when the file tree is scanned. File content
is read directly from the source files when the corresponding data
sectors are requested — never copied into a buffer.

Inspired by vsFat (GPL-2.0, C) — reimplemented in Python with a
different architecture (no BUSE dependency, NBD server instead).
"""

from __future__ import annotations

import asyncio
import logging
import math
import os
import struct
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.virtual_media")

SECTOR_SIZE = 512
CLUSTER_SIZE = 4096  # 8 sectors per cluster (standard for FAT32)
SECTORS_PER_CLUSTER = CLUSTER_SIZE // SECTOR_SIZE
FAT_ENTRY_SIZE = 4  # FAT32 uses 32-bit entries
RESERVED_SECTORS = 32
NUM_FATS = 2


# ── FAT32 synthesiser ─────────────────────────────────────────────────────────

@dataclass
class FileEntry:
    """A file or directory in the virtual filesystem."""
    name: str               # 8.3 or long name
    path: Path              # host filesystem path
    is_dir: bool
    size: int = 0
    cluster: int = 0        # starting cluster (assigned during scan)
    children: list["FileEntry"] = field(default_factory=list)
    short_name: str = ""    # 8.3 name for FAT directory entry
    ctime: float = 0.0


class FATSynthesiser:
    """
    Synthesise a FAT32 block device from a directory tree.

    Call scan() to index the source directory. After that, read_sectors()
    returns the correct bytes for any sector offset — boot sector, FAT
    tables, directory entries, or file data.

    No image file is created. Everything is computed on demand.
    """

    def __init__(self, source_dir: str | Path, label: str = "OZMA",
                 total_size_mb: int = 0, watch: bool = True) -> None:
        self._source = Path(source_dir)
        self._label = label[:11].upper().ljust(11)
        self._root = FileEntry(name="", path=self._source, is_dir=True)
        self._files: list[FileEntry] = []  # all files, flat
        self._next_cluster = 3  # cluster 2 = root directory, 0,1 reserved
        self._total_sectors = 0
        self._fat_sectors = 0
        self._data_start_sector = 0
        self._total_size_mb = total_size_mb
        self._scanned = False
        self._short_names: set[str] = set()  # track assigned 8.3 names for collision avoidance
        self._watch = watch
        self._watcher_thread: threading.Thread | None = None
        self._watcher_stop = threading.Event()
        self._lock = threading.Lock()  # protects _root, _files, _next_cluster during rescan

    def scan(self) -> None:
        """Scan the source directory and build the FAT32 layout."""
        with self._lock:
            self._files = []
            self._short_names = set()
            self._next_cluster = 3  # cluster 2 = root directory
            self._root = FileEntry(name="", path=self._source, is_dir=True)
            self._scan_dir(self._root, self._source)

            # Calculate geometry — use generous headroom so rescans don't change geometry
            total_data_bytes = sum(f.size for f in self._files if not f.is_dir)
            total_data_bytes += len(self._files) * 512
            # Add 20% headroom + 10MB for new files to appear without geometry change
            total_data_bytes = int(total_data_bytes * 1.2) + 10 * 1024 * 1024
            total_clusters = max(total_data_bytes // CLUSTER_SIZE + 1, 65536)

            if self._total_size_mb > 0:
                total_clusters = max(total_clusters, self._total_size_mb * 1024 * 1024 // CLUSTER_SIZE)

            self._fat_sectors = math.ceil(total_clusters * FAT_ENTRY_SIZE / SECTOR_SIZE)
            self._data_start_sector = RESERVED_SECTORS + (self._fat_sectors * NUM_FATS)
            self._total_sectors = self._data_start_sector + (total_clusters * SECTORS_PER_CLUSTER)
            self._total_clusters = total_clusters

            self._scanned = True
            log.info("FAT32 synthesised: %d files, %d clusters, %dMB virtual",
                     len(self._files), total_clusters,
                     self._total_sectors * SECTOR_SIZE // 1_048_576)

        # Start file watcher if requested
        if self._watch and not self._watcher_thread:
            self._start_watcher()

    def rescan(self) -> None:
        """
        Re-scan the source directory for new/changed/deleted files.

        Preserves the overall geometry (total_sectors, fat_sectors) so that
        the USB host doesn't see a capacity change. Only updates file entries,
        cluster assignments, and directory structures.
        """
        old_total = self._total_sectors
        old_fat = self._fat_sectors
        old_data_start = self._data_start_sector
        old_clusters = getattr(self, '_total_clusters', 0)

        with self._lock:
            self._files = []
            self._short_names = set()
            self._next_cluster = 3
            self._root = FileEntry(name="", path=self._source, is_dir=True)
            self._scan_dir(self._root, self._source)

            # Keep geometry stable — only grow if we absolutely must
            total_data_bytes = sum(f.size for f in self._files if not f.is_dir)
            total_data_bytes += len(self._files) * 512
            needed_clusters = total_data_bytes // CLUSTER_SIZE + 1

            if needed_clusters > old_clusters:
                # Must grow — recalculate geometry
                total_clusters = max(int(needed_clusters * 1.2), old_clusters)
                self._fat_sectors = math.ceil(total_clusters * FAT_ENTRY_SIZE / SECTOR_SIZE)
                self._data_start_sector = RESERVED_SECTORS + (self._fat_sectors * NUM_FATS)
                self._total_sectors = self._data_start_sector + (total_clusters * SECTORS_PER_CLUSTER)
                self._total_clusters = total_clusters
                log.info("FAT32 rescan: geometry grew to %d clusters", total_clusters)
            else:
                # Keep old geometry
                self._total_sectors = old_total
                self._fat_sectors = old_fat
                self._data_start_sector = old_data_start
                self._total_clusters = old_clusters

        log.info("FAT32 rescan: %d files", len(self._files))

    def _start_watcher(self) -> None:
        """Start a background thread that watches for file changes."""
        self._watcher_stop.clear()
        self._watcher_thread = threading.Thread(
            target=self._watch_loop, daemon=True, name="fat32-watcher"
        )
        self._watcher_thread.start()
        log.info("File watcher started for %s", self._source)

    def _watch_loop(self) -> None:
        """Poll for file changes (works everywhere, no inotify dependency)."""
        # Build initial snapshot: path → (mtime, size)
        def snapshot() -> dict[str, tuple[float, int]]:
            result = {}
            try:
                for root, dirs, files in os.walk(self._source):
                    # Skip hidden dirs
                    dirs[:] = [d for d in dirs if not d.startswith(".")]
                    for f in files:
                        if f.startswith("."):
                            continue
                        p = os.path.join(root, f)
                        try:
                            st = os.stat(p)
                            result[p] = (st.st_mtime, st.st_size)
                        except OSError:
                            pass
            except OSError:
                pass
            return result

        prev = snapshot()

        while not self._watcher_stop.wait(timeout=1.0):
            curr = snapshot()
            if curr != prev:
                # Something changed — new files, modified files, or deleted files
                added = set(curr) - set(prev)
                removed = set(prev) - set(curr)
                modified = {p for p in curr if p in prev and curr[p] != prev[p]}

                if added or removed or modified:
                    changes = []
                    if added:
                        changes.append(f"+{len(added)}")
                    if removed:
                        changes.append(f"-{len(removed)}")
                    if modified:
                        changes.append(f"~{len(modified)}")
                    log.info("File change detected (%s), rescanning", " ".join(changes))
                    self.rescan()

                prev = curr

    def stop_watcher(self) -> None:
        """Stop the file watcher thread."""
        self._watcher_stop.set()
        if self._watcher_thread:
            self._watcher_thread.join(timeout=5)
            self._watcher_thread = None

    @property
    def total_bytes(self) -> int:
        return self._total_sectors * SECTOR_SIZE

    def _scan_dir(self, parent: FileEntry, host_path: Path) -> None:
        """Recursively scan a directory and assign clusters."""
        try:
            entries = sorted(host_path.iterdir(), key=lambda p: p.name.lower())
        except PermissionError:
            return

        children = []
        for item in entries:
            if item.name.startswith("."):
                continue  # skip hidden files
            try:
                st = item.stat()
            except OSError:
                continue

            entry = FileEntry(
                name=item.name,
                path=item,
                is_dir=item.is_dir(),
                size=st.st_size if item.is_file() else 0,
                cluster=self._next_cluster,
                short_name=self._make_short_name(item.name),
                ctime=st.st_mtime,
            )
            children.append(entry)

            # Allocate clusters
            if entry.is_dir:
                # Estimate dir entries needed (each child ~3 entries with LFN, 32 bytes each)
                try:
                    child_count = sum(1 for _ in item.iterdir() if not _.name.startswith("."))
                except OSError:
                    child_count = 0
                dir_bytes = (child_count * 3 + 2) * 32  # +2 for . and ..
                dir_clusters = max(1, math.ceil(dir_bytes / CLUSTER_SIZE))
                self._next_cluster += dir_clusters
            elif entry.size > 0:
                clusters_needed = math.ceil(entry.size / CLUSTER_SIZE)
                self._next_cluster += clusters_needed
            else:
                self._next_cluster += 1  # empty file still gets a cluster

            parent.children.append(entry)
            self._files.append(entry)

        # Recurse into subdirectories after allocating all entries
        for entry in children:
            if entry.is_dir:
                self._scan_dir(entry, entry.path)

    # ── Sector reading ─────────────────────────────────────────────────

    def read_sectors(self, offset: int, length: int) -> bytes:
        """
        Read bytes from the virtual block device.

        This is the core method. Given a byte offset and length, return
        the correct data — whether it's the boot sector, FAT table,
        a directory entry, or file content.

        Thread-safe — the lock protects against concurrent rescans.
        """
        if not self._scanned:
            self.scan()

        result = bytearray(length)
        pos = 0

        with self._lock:
            while pos < length:
                sector = (offset + pos) // SECTOR_SIZE
                sector_offset = (offset + pos) % SECTOR_SIZE
                remaining = length - pos
                chunk_len = min(SECTOR_SIZE - sector_offset, remaining)

                sector_data = self._read_sector(sector)
                result[pos:pos + chunk_len] = sector_data[sector_offset:sector_offset + chunk_len]
                pos += chunk_len

        return bytes(result)

    def _read_sector(self, sector: int) -> bytes:
        """Read a single sector by number."""
        if sector == 0:
            return self._boot_sector()
        elif sector < RESERVED_SECTORS:
            if sector == 1:
                return self._fsinfo_sector()
            return b"\x00" * SECTOR_SIZE  # other reserved sectors
        elif sector < RESERVED_SECTORS + self._fat_sectors:
            return self._fat_sector(sector - RESERVED_SECTORS)
        elif sector < RESERVED_SECTORS + self._fat_sectors * 2:
            # Second FAT copy — identical to first
            return self._fat_sector(sector - RESERVED_SECTORS - self._fat_sectors)
        elif sector >= self._data_start_sector:
            return self._data_sector(sector - self._data_start_sector)
        return b"\x00" * SECTOR_SIZE

    # ── Boot sector ────────────────────────────────────────────────────

    def _boot_sector(self) -> bytes:
        """Generate the FAT32 boot sector (sector 0)."""
        bs = bytearray(SECTOR_SIZE)
        # Jump instruction
        bs[0:3] = b"\xEB\x58\x90"
        # OEM name
        bs[3:11] = b"OZMA    "
        # BPB (BIOS Parameter Block)
        struct.pack_into("<H", bs, 11, SECTOR_SIZE)           # bytes per sector
        bs[13] = SECTORS_PER_CLUSTER                           # sectors per cluster
        struct.pack_into("<H", bs, 14, RESERVED_SECTORS)       # reserved sectors
        bs[16] = NUM_FATS                                      # number of FATs
        struct.pack_into("<H", bs, 17, 0)                      # root entry count (0 for FAT32)
        struct.pack_into("<H", bs, 19, 0)                      # total sectors 16 (0 for FAT32)
        bs[21] = 0xF8                                          # media type (fixed disk)
        struct.pack_into("<H", bs, 22, 0)                      # FAT size 16 (0 for FAT32)
        struct.pack_into("<H", bs, 24, 63)                     # sectors per track
        struct.pack_into("<H", bs, 26, 255)                    # number of heads
        struct.pack_into("<I", bs, 28, 0)                      # hidden sectors
        struct.pack_into("<I", bs, 32, self._total_sectors)    # total sectors 32
        # FAT32 extended BPB
        struct.pack_into("<I", bs, 36, self._fat_sectors)      # FAT size 32
        struct.pack_into("<H", bs, 40, 0)                      # ext flags
        struct.pack_into("<H", bs, 42, 0)                      # FS version
        struct.pack_into("<I", bs, 44, 2)                      # root cluster
        struct.pack_into("<H", bs, 48, 1)                      # FSInfo sector
        struct.pack_into("<H", bs, 50, 6)                      # backup boot sector
        bs[66] = 0x29                                          # boot signature
        struct.pack_into("<I", bs, 67, 0x12345678)             # volume serial
        bs[71:82] = self._label.encode("ascii")[:11]           # volume label
        bs[82:90] = b"FAT32   "                                # FS type
        # Boot signature
        bs[510] = 0x55
        bs[511] = 0xAA
        return bytes(bs)

    def _fsinfo_sector(self) -> bytes:
        """Generate the FSInfo sector (sector 1)."""
        fs = bytearray(SECTOR_SIZE)
        struct.pack_into("<I", fs, 0, 0x41615252)    # lead signature
        struct.pack_into("<I", fs, 484, 0x61417272)   # struct signature
        struct.pack_into("<I", fs, 488, 0xFFFFFFFF)   # free cluster count (unknown)
        struct.pack_into("<I", fs, 492, 0xFFFFFFFF)   # next free cluster (unknown)
        fs[510] = 0x55
        fs[511] = 0xAA
        return bytes(fs)

    # ── FAT table ──────────────────────────────────────────────────────

    def _fat_sector(self, fat_sector_index: int) -> bytes:
        """Generate a sector of the FAT table."""
        data = bytearray(SECTOR_SIZE)
        entries_per_sector = SECTOR_SIZE // FAT_ENTRY_SIZE
        start_entry = fat_sector_index * entries_per_sector

        for i in range(entries_per_sector):
            entry_idx = start_entry + i
            value = self._fat_entry(entry_idx)
            struct.pack_into("<I", data, i * FAT_ENTRY_SIZE, value & 0x0FFFFFFF)

        return bytes(data)

    def _fat_entry(self, cluster: int) -> int:
        """Get the FAT entry for a cluster."""
        if cluster == 0:
            return 0x0FFFFFF8  # media type
        if cluster == 1:
            return 0x0FFFFFFF  # end of chain marker

        # Root directory cluster(s)
        if cluster == 2:
            root_clusters = self._dir_clusters(self._root)
            if root_clusters > 1:
                return 3  # next cluster in chain
            return 0x0FFFFFFF

        # Find which file/dir owns this cluster
        for f in self._files:
            if f.cluster == 0:
                continue
            if f.is_dir:
                dir_clusters = self._dir_clusters(f)
                if f.cluster <= cluster < f.cluster + dir_clusters:
                    if cluster == f.cluster + dir_clusters - 1:
                        return 0x0FFFFFFF  # last cluster, end of chain
                    return cluster + 1
            else:
                clusters_needed = max(1, math.ceil(f.size / CLUSTER_SIZE))
                if f.cluster <= cluster < f.cluster + clusters_needed:
                    if cluster == f.cluster + clusters_needed - 1:
                        return 0x0FFFFFFF  # last cluster, end of chain
                    return cluster + 1  # next cluster in chain

        return 0  # free cluster

    # ── Data region ────────────────────────────────────────────────────

    def _dir_clusters(self, dir_entry: FileEntry) -> int:
        """How many clusters this directory occupies."""
        dir_bytes = len(self._dir_data(dir_entry))
        return max(1, math.ceil(dir_bytes / CLUSTER_SIZE))

    def _data_sector(self, data_sector_offset: int) -> bytes:
        """Read a sector from the data region."""
        cluster = data_sector_offset // SECTORS_PER_CLUSTER + 2  # clusters start at 2
        sector_in_cluster = data_sector_offset % SECTORS_PER_CLUSTER

        # Root directory (cluster 2)
        if cluster == 2:
            return self._dir_sector(self._root, sector_in_cluster)

        # Find which file/dir owns this cluster
        for f in self._files:
            if f.cluster == 0:
                continue
            if f.is_dir:
                dir_clusters = self._dir_clusters(f)
                if f.cluster <= cluster < f.cluster + dir_clusters:
                    # Sector offset within the full directory data
                    offset_in_dir = (cluster - f.cluster) * SECTORS_PER_CLUSTER + sector_in_cluster
                    return self._dir_sector(f, offset_in_dir)
            else:
                clusters_needed = max(1, math.ceil(f.size / CLUSTER_SIZE))
                if f.cluster <= cluster < f.cluster + clusters_needed:
                    file_offset = (cluster - f.cluster) * CLUSTER_SIZE + sector_in_cluster * SECTOR_SIZE
                    return self._read_file_sector(f, file_offset)

        return b"\x00" * SECTOR_SIZE

    def _dir_data(self, dir_entry: FileEntry) -> bytes:
        """Generate all directory entry bytes for a directory."""
        entries = bytearray()

        if dir_entry is self._root:
            # Root: volume label entry
            entries += self._make_dir_entry(self._label, 0x08, 0, 0)
        else:
            # Subdirectory: . and .. entries
            entries += self._make_dir_entry(".          ", 0x10, dir_entry.cluster, 0)
            parent_cluster = 0
            for f in self._files:
                if f.is_dir and dir_entry in f.children:
                    parent_cluster = f.cluster
                    break
            entries += self._make_dir_entry("..         ", 0x10, parent_cluster, 0)

        for child in dir_entry.children:
            base_83 = child.short_name[:8].rstrip()
            ext_83 = child.short_name[8:11].rstrip()
            name_83 = f"{base_83}.{ext_83}" if ext_83 else base_83
            needs_lfn = child.name.upper() != name_83
            entries += self._make_dir_entry(
                child.short_name, 0x10 if child.is_dir else 0x20,
                child.cluster, child.size,
                long_name=child.name if needs_lfn else "",
            )

        return bytes(entries)

    def _dir_sector(self, dir_entry: FileEntry, data_sector_offset: int) -> bytes:
        """Generate a directory listing sector at the given byte offset within the dir."""
        all_entries = self._dir_data(dir_entry)
        offset = data_sector_offset * SECTOR_SIZE
        if offset >= len(all_entries):
            return b"\x00" * SECTOR_SIZE
        sector_data = all_entries[offset:offset + SECTOR_SIZE]
        return bytes(sector_data.ljust(SECTOR_SIZE, b"\x00"))

    def _read_file_sector(self, entry: FileEntry, file_offset: int) -> bytes:
        """Read a sector of file content from the actual host file."""
        try:
            with open(entry.path, "rb") as f:
                f.seek(file_offset)
                data = f.read(SECTOR_SIZE)
                if len(data) < SECTOR_SIZE:
                    data += b"\x00" * (SECTOR_SIZE - len(data))
                return data
        except Exception:
            return b"\x00" * SECTOR_SIZE

    # ── Directory entries ──────────────────────────────────────────────

    def _make_dir_entry(self, name: str, attr: int, cluster: int, size: int,
                          long_name: str = "") -> bytes:
        """Create FAT32 directory entries, with optional VFAT long filename."""
        result = bytearray()

        # If the name needs LFN entries (doesn't fit 8.3, or has mixed case)
        if long_name and len(long_name) > 0:
            # Create LFN entries (before the 8.3 entry, in reverse order)
            lfn_entries = self._make_lfn_entries(long_name, self._lfn_checksum(name))
            result += lfn_entries

        # 8.3 entry
        entry = bytearray(32)
        short = name.ljust(11)[:11].encode("ascii", errors="replace")
        entry[0:11] = short
        entry[11] = attr
        entry[14:16] = struct.pack("<H", 0x6000)
        entry[16:18] = struct.pack("<H", 0x5A21)
        entry[18:20] = struct.pack("<H", 0x5A21)
        entry[20:22] = struct.pack("<H", cluster >> 16)
        entry[22:24] = struct.pack("<H", 0x6000)
        entry[24:26] = struct.pack("<H", 0x5A21)
        entry[26:28] = struct.pack("<H", cluster & 0xFFFF)
        struct.pack_into("<I", entry, 28, size)
        result += entry

        return bytes(result)

    def _make_lfn_entries(self, long_name: str, checksum: int) -> bytes:
        """Create VFAT long filename directory entries."""
        # Pad name to multiple of 13 with 0xFFFF
        encoded = long_name.encode("utf-16-le")
        # Each LFN entry holds 13 UTF-16 characters (26 bytes)
        chars_per_entry = 13
        # Pad with 0x00 0x00 terminator then 0xFF 0xFF
        padded = long_name + "\x00"
        while len(padded) % chars_per_entry != 0:
            padded += "\xff"

        num_entries = (len(padded) + chars_per_entry - 1) // chars_per_entry
        result = bytearray()

        # LFN entries are stored in reverse order
        for i in range(num_entries, 0, -1):
            entry = bytearray(32)
            seq = i
            if i == num_entries:
                seq |= 0x40  # last LFN entry flag

            chunk = padded[(i - 1) * chars_per_entry:i * chars_per_entry]
            utf16 = chunk.encode("utf-16-le")
            # Pad to 26 bytes
            while len(utf16) < 26:
                utf16 += b"\xff\xff"

            entry[0] = seq                    # sequence number
            entry[1:11] = utf16[0:10]         # chars 1-5
            entry[11] = 0x0F                  # LFN attribute
            entry[12] = 0x00                  # type
            entry[13] = checksum              # 8.3 checksum
            entry[14:26] = utf16[10:22]       # chars 6-11
            entry[26:28] = b"\x00\x00"        # cluster (always 0 for LFN)
            entry[28:32] = utf16[22:26]       # chars 12-13

            result += entry

        return bytes(result)

    @staticmethod
    def _lfn_checksum(short_name: str) -> int:
        """Compute the checksum for LFN entries from the 8.3 name."""
        name = short_name.ljust(11)[:11].encode("ascii", errors="replace")
        chk = 0
        for b in name:
            chk = ((chk >> 1) + ((chk & 1) << 7) + b) & 0xFF
        return chk

    def _make_short_name(self, name: str) -> str:
        """Convert a filename to 8.3 format with ~N suffix for collisions."""
        name_upper = name.upper()
        # Split name and extension
        if "." in name_upper:
            base, ext = name_upper.rsplit(".", 1)
        else:
            base, ext = name_upper, ""

        # Clean characters (replace invalid 8.3 chars)
        base = "".join(c if c.isalnum() or c in "_" else "_" for c in base)
        ext = "".join(c if c.isalnum() or c in "_" else "_" for c in ext)[:3]

        # Try the exact name first if it fits 8.3
        if len(base) <= 8 and len(ext) <= 3:
            candidate = f"{base:<8}{ext:<3}"
            if candidate not in self._short_names:
                self._short_names.add(candidate)
                return candidate

        # Need a ~N suffix — try incrementing until unique
        for n in range(1, 1000):
            suffix = f"~{n}"
            max_base = 8 - len(suffix)
            short_base = base[:max_base] + suffix
            candidate = f"{short_base:<8}{ext:<3}"
            if candidate not in self._short_names:
                self._short_names.add(candidate)
                return candidate

        # Fallback (should never happen)
        candidate = f"{'FILE~999':<8}{ext:<3}"
        self._short_names.add(candidate)
        return candidate


# ── NBD server ─────────────────────────────────────────────────────────────────

NBD_MAGIC = b"NBDMAGIC"
NBD_CLISERV_MAGIC = b"\x00\x00\x42\x02\x81\x86\x12\x53"
NBD_REQUEST_MAGIC = 0x25609513
NBD_REPLY_MAGIC = b"\x67\x44\x66\x98"
NBD_CMD_READ = 0
NBD_CMD_WRITE = 1
NBD_CMD_DISC = 2


class NBDServer:
    """
    NBD (Network Block Device) server backed by a FATSynthesiser.

    QEMU connects to this and sees a FAT32 disk. No image file.

    Usage:
        synth = FATSynthesiser("/path/to/files")
        synth.scan()
        server = NBDServer(synth, port=10809)
        await server.start()
        # QEMU: -drive driver=nbd,host=localhost,port=10809,id=usb0
    """

    def __init__(self, synth: FATSynthesiser, host: str = "127.0.0.1",
                 port: int = 10809, writable: bool = True) -> None:
        self._synth = synth
        self._host = host
        self._port = port
        self._writable = writable
        self._server: asyncio.Server | None = None
        # Write overlay: sector_offset → data. Writes go here instead of
        # the synthesiser. Reads check the overlay first, then the synth.
        self._write_overlay: dict[int, bytes] = {}

    async def start(self) -> None:
        self._server = await asyncio.start_server(
            self._handle_client, self._host, self._port,
        )
        log.info("NBD server listening on %s:%d (%dMB virtual FAT32)",
                 self._host, self._port,
                 self._synth.total_bytes // 1_048_576)

    async def stop(self) -> None:
        if self._server:
            self._server.close()
            await self._server.wait_closed()

    def _read(self, offset: int, length: int) -> bytes:
        """Read with write overlay — overlay wins over synthesiser."""
        result = bytearray(self._synth.read_sectors(offset, length))
        # Apply any overlaid writes
        for ov_offset, ov_data in self._write_overlay.items():
            ov_end = ov_offset + len(ov_data)
            req_end = offset + length
            # Check for overlap
            if ov_offset < req_end and ov_end > offset:
                # Calculate the overlapping region
                start = max(ov_offset, offset)
                end = min(ov_end, req_end)
                result[start - offset:end - offset] = ov_data[start - ov_offset:end - ov_offset]
        return bytes(result)

    def _write(self, offset: int, data: bytes) -> None:
        """Store a write in the overlay."""
        self._write_overlay[offset] = data

    async def _handle_client(self, reader: asyncio.StreamReader,
                               writer: asyncio.StreamWriter) -> None:
        """Handle an NBD client connection."""
        peer = writer.get_extra_info("peername")
        log.info("NBD client connected: %s", peer)

        try:
            # Handshake: send magic + size + flags
            size = self._synth.total_bytes
            handshake = NBD_MAGIC + NBD_CLISERV_MAGIC
            handshake += struct.pack(">Q", size)
            handshake += b"\x00" * 128  # flags + padding
            writer.write(handshake)
            await writer.drain()

            # Request loop
            while True:
                header = await reader.readexactly(28)
                magic, cmd_type, handle, offset, length = struct.unpack(">IIQQI", header)

                if magic != NBD_REQUEST_MAGIC:
                    log.warning("Bad NBD magic: 0x%08x", magic)
                    break

                if cmd_type == NBD_CMD_READ:
                    data = self._read(offset, length)
                    reply = NBD_REPLY_MAGIC + b"\x00\x00\x00\x00" + struct.pack(">Q", handle)
                    writer.write(reply + data)
                    await writer.drain()

                elif cmd_type == NBD_CMD_WRITE:
                    write_data = await reader.readexactly(length)
                    if self._writable:
                        self._write(offset, write_data)
                    reply = NBD_REPLY_MAGIC + b"\x00\x00\x00\x00" + struct.pack(">Q", handle)
                    writer.write(reply)
                    await writer.drain()

                elif cmd_type == NBD_CMD_DISC:
                    log.info("NBD client disconnected: %s", peer)
                    break

                elif cmd_type == 5:  # NBD_CMD_FLUSH
                    reply = NBD_REPLY_MAGIC + b"\x00\x00\x00\x00" + struct.pack(">Q", handle)
                    writer.write(reply)
                    await writer.drain()

        except (asyncio.IncompleteReadError, ConnectionResetError):
            log.info("NBD client disconnected: %s", peer)
        except Exception as e:
            log.error("NBD error: %s", e)
        finally:
            writer.close()


# ── Convenience ────────────────────────────────────────────────────────────────

async def serve_directory(source_dir: str | Path, port: int = 10809,
                           label: str = "OZMA") -> NBDServer:
    """
    Serve a directory as a virtual FAT32 block device via NBD.

    For hypervisors that support NBD natively. QEMU connects with:
      -drive driver=nbd,host=localhost,port=10809,if=none,id=usb0
      -device usb-storage,drive=usb0
    """
    synth = FATSynthesiser(source_dir, label=label)
    synth.scan()
    server = NBDServer(synth, port=port)
    await server.start()
    return server


def synthesise_to_file(source_dir: str | Path, output_path: str | Path,
                        label: str = "OZMA") -> Path:
    """
    Synthesise a FAT32 image file from a directory. Pure Python, no root.

    This is the primary path for USB mass storage:
      - Soft node: synthesise → attach via QMP usb-storage
      - Hardware node: synthesise → back the USB gadget

    The image is written sector by sector from the synthesiser — no
    mkfs.vfat, no mount, no sudo, no mtools. Just Python.
    """
    output = Path(output_path)
    synth = FATSynthesiser(source_dir, label=label)
    synth.scan()

    total = synth.total_bytes
    written = 0
    chunk_size = 1024 * 1024  # write 1MB at a time

    with open(output, "wb") as f:
        while written < total:
            size = min(chunk_size, total - written)
            data = synth.read_sectors(written, size)
            f.write(data)
            written += size

    log.info("Synthesised FAT32 image: %s (%dMB, %s)",
             output, total // 1_048_576, label)
    return output


async def attach_directory_as_usb(qmp: Any, source_dir: str | Path,
                                    drive_id: str = "ozma-media",
                                    label: str = "OZMA",
                                    readonly: bool = False,
                                    work_dir: str | Path = "/tmp") -> Path | None:
    """
    Synthesise a directory as FAT32 and attach as USB mass storage to a VM.

    Read-write by default. The VM can write files to the drive. After
    detaching, use sync_image_to_directory() to extract new/changed files.

    Returns the image path, or None on failure.
    """
    image_path = Path(work_dir) / f"{drive_id}.img"
    synthesise_to_file(source_dir, image_path, label=label)
    ok = await qmp.attach_usb_storage(str(image_path), drive_id, readonly=readonly)
    if ok:
        mode = "read-only" if readonly else "read-write"
        log.info("USB media attached (%s): %s → %s", mode, source_dir, drive_id)
        return image_path
    else:
        log.error("Failed to attach USB media via QMP")
        image_path.unlink(missing_ok=True)
        return None


def sync_image_to_directory(image_path: str | Path, dest_dir: str | Path) -> list[str]:
    """
    Extract files from a FAT32 image back to a host directory.

    Call this after detaching a read-write USB drive to get files
    the VM wrote. Uses mtools (no root/mount needed) or mount fallback.

    Returns list of extracted file paths.
    """
    import shutil
    import subprocess

    image = Path(image_path)
    dest = Path(dest_dir)
    dest.mkdir(parents=True, exist_ok=True)
    extracted: list[str] = []

    if shutil.which("mcopy"):
        # mtools: extract everything without mounting
        env = {**os.environ, "MTOOLS_SKIP_CHECK": "1"}
        try:
            result = subprocess.run(
                ["mdir", "-i", str(image), "-b", "::"],
                capture_output=True, text=True, env=env, timeout=10,
            )
            for line in result.stdout.splitlines():
                line = line.strip().lstrip("::/")
                if not line or line.startswith("."):
                    continue
                out_path = dest / line
                out_path.parent.mkdir(parents=True, exist_ok=True)
                cp = subprocess.run(
                    ["mcopy", "-i", str(image), f"::{line}", str(out_path)],
                    capture_output=True, env=env, timeout=30,
                )
                if cp.returncode == 0:
                    extracted.append(str(out_path))
        except Exception as e:
            log.warning("mtools extract failed: %s", e)
    else:
        # Fallback: mount (needs sudo)
        import tempfile
        mnt = Path(tempfile.mkdtemp())
        try:
            subprocess.run(["sudo", "mount", "-o", "loop,ro", str(image), str(mnt)],
                           check=True, capture_output=True, timeout=10)
            for item in mnt.rglob("*"):
                if item.is_file():
                    rel = item.relative_to(mnt)
                    out_path = dest / rel
                    out_path.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copy2(item, out_path)
                    extracted.append(str(out_path))
        except Exception as e:
            log.warning("Mount extract failed: %s", e)
        finally:
            subprocess.run(["sudo", "umount", str(mnt)], capture_output=True)
            mnt.rmdir()

    if extracted:
        log.info("Extracted %d files from image to %s", len(extracted), dest)
    return extracted
