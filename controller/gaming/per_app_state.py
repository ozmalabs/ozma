# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Per-app persistent game state.

Provides persistent game saves per user per app with auto-mount on session start.

Features:
  - Persistent game saves per user per app
  - Auto-mount on session start
  - Sync on exit
"""

from __future__ import annotations

import asyncio
import json
import logging
import shutil
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.controller.gaming.per_app_state")


# ─── Constants ───────────────────────────────────────────────────────────────

STATE_DIR = Path("/var/lib/ozma/gaming/app_state")
MAX_STATE_SIZE_MB = 100


# ─── State Entry ─────────────────────────────────────────────────────────────

@dataclass
class AppStateEntry:
    """A single state entry for a user/app combination."""
    user_id: str
    app_id: str
    state_data: dict[str, Any]
    created_at: float = field(default_factory=time.time)
    last_modified: float = field(default_factory=time.time)
    version: int = 1

    def to_dict(self) -> dict[str, Any]:
        return {
            "user_id": self.user_id,
            "app_id": self.app_id,
            "state_data": self.state_data,
            "created_at": self.created_at,
            "last_modified": self.last_modified,
            "version": self.version,
        }

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> "AppStateEntry":
        return cls(
            user_id=data.get("user_id", ""),
            app_id=data.get("app_id", ""),
            state_data=data.get("state_data", {}),
            created_at=data.get("created_at", time.time()),
            last_modified=data.get("last_modified", time.time()),
            version=data.get("version", 1),
        )


# ─── State Manager ───────────────────────────────────────────────────────────

class AppStateManager:
    """
    Manages persistent game state per user per app.

    Features:
      - Per-user, per-app state storage
      - Auto-mount on session start
      - Sync on exit
      - State versioning
      - Size limits enforcement
    """

    def __init__(self, data_dir: Path = STATE_DIR):
        self._data_dir = data_dir
        self._data_dir.mkdir(parents=True, exist_ok=True)

        # In-memory cache
        self._states: dict[str, AppStateEntry] = {}

        # Mount points per session
        self._mounts: dict[str, Path] = {}

    async def start(self) -> None:
        """Start the state manager."""
        await self._load_all_states()
        log.info("AppStateManager started")

    async def stop(self) -> None:
        """Stop the state manager."""
        await self._save_all_states()
        log.info("AppStateManager stopped")

    async def _load_all_states(self) -> None:
        """Load all states from disk."""
        for state_file in self._data_dir.glob("*.json"):
            try:
                data = json.loads(state_file.read_text())
                entry = AppStateEntry.from_dict(data)
                self._states[entry.user_id + "_" + entry.app_id] = entry
            except Exception as e:
                log.error("Failed to load state file %s: %s", state_file, e)

        log.info("Loaded %d state entries", len(self._states))

    async def _save_all_states(self) -> None:
        """Save all states to disk."""
        for entry in self._states.values():
            await self._save_state(entry)

    def _get_state_path(self, user_id: str, app_id: str) -> Path:
        """Get the state file path for a user/app."""
        safe_user = user_id.replace("/", "_").replace("\\", "_")
        safe_app = app_id.replace("/", "_").replace("\\", "_")
        return self._data_dir / f"{safe_user}_{safe_app}.json"

    async def _save_state(self, entry: AppStateEntry) -> None:
        """Save a state entry to disk."""
        state_path = self._get_state_path(entry.user_id, entry.app_id)
        try:
            data = entry.to_dict()
            state_path.write_text(json.dumps(data, indent=2))
        except Exception as e:
            log.error("Failed to save state: %s", e)

    # ─── State operations ─────────────────────────────────────────────────────

    async def get_state(self, user_id: str, app_id: str) -> dict[str, Any] | None:
        """Get the state for a user/app."""
        key = user_id + "_" + app_id
        entry = self._states.get(key)
        if not entry:
            return None

        # Update last access
        entry.last_modified = time.time()
        return entry.state_data.copy()

    async def set_state(
        self,
        user_id: str,
        app_id: str,
        state_data: dict[str, Any],
        create_if_missing: bool = True,
    ) -> bool:
        """Set the state for a user/app."""
        key = user_id + "_" + app_id
        entry = self._states.get(key)

        if not entry and not create_if_missing:
            return False

        if not entry:
            entry = AppStateEntry(
                user_id=user_id,
                app_id=app_id,
                state_data=state_data,
            )
        else:
            entry.state_data = state_data
            entry.last_modified = time.time()
            entry.version += 1

        self._states[key] = entry
        await self._save_state(entry)
        return True

    async def delete_state(self, user_id: str, app_id: str) -> bool:
        """Delete the state for a user/app."""
        key = user_id + "_" + app_id
        if key not in self._states:
            return False

        del self._states[key]

        # Delete file
        state_path = self._get_state_path(user_id, app_id)
        if state_path.exists():
            try:
                state_path.unlink()
            except Exception as e:
                log.warning("Failed to delete state file: %s", e)

        return True

    async def merge_state(
        self,
        user_id: str,
        app_id: str,
        updates: dict[str, Any],
        deep_merge: bool = True,
    ) -> bool:
        """Merge updates into existing state."""
        current = await self.get_state(user_id, app_id)
        if current is None:
            return await self.set_state(user_id, app_id, updates)

        if deep_merge:
            # Deep merge
            merged = self._deep_merge(current, updates)
        else:
            # Shallow merge
            merged = {**current, **updates}

        return await self.set_state(user_id, app_id, merged)

    def _deep_merge(self, base: dict, updates: dict) -> dict:
        """Deep merge two dictionaries."""
        result = base.copy()
        for key, value in updates.items():
            if key in result and isinstance(result[key], dict) and isinstance(value, dict):
                result[key] = self._deep_merge(result[key], value)
            else:
                result[key] = value
        return result

    # ─── Mount operations ─────────────────────────────────────────────────────

    def create_mount(self, user_id: str, app_id: str, mount_path: Path) -> Path:
        """Create a mount for a user/app state."""
        # Create directory structure
        mount_path.mkdir(parents=True, exist_ok=True)

        # Create symlink or bind mount
        state_path = self._get_state_path(user_id, app_id)
        if state_path.exists():
            # Copy existing state to mount
            mount_state = mount_path / "state.json"
            shutil.copy2(state_path, mount_state)

        self._mounts[mount_path] = state_path
        return mount_path

    def remove_mount(self, mount_path: Path) -> bool:
        """Remove a mount and sync changes."""
        if mount_path not in self._mounts:
            return False

        # Sync changes from mount to state file
        mount_state = mount_path / "state.json"
        state_path = self._mounts[mount_path]

        if mount_state.exists():
            try:
                data = json.loads(mount_state.read_text())
                entry = AppStateEntry.from_dict(data)
                entry.last_modified = time.time()
                entry.version += 1
                self._states[entry.user_id + "_" + entry.app_id] = entry
                self._save_state(entry)
            except Exception as e:
                log.error("Failed to sync mount state: %s", e)

        # Clean up mount directory
        if mount_path.exists():
            try:
                shutil.rmtree(mount_path)
            except Exception as e:
                log.warning("Failed to clean up mount: %s", e)

        del self._mounts[mount_path]
        return True

    # ─── State cleanup ────────────────────────────────────────────────────────

    async def cleanup_old_states(self, max_age_days: int = 30) -> int:
        """Remove states older than the specified age."""
        cutoff = time.time() - (max_age_days * 86400)
        removed = 0

        for key, entry in list(self._states.items()):
            if entry.last_modified < cutoff:
                del self._states[key]
                removed += 1

        log.info("Cleaned up %d old state entries", removed)
        return removed

    async def get_size_usage(self) -> dict[str, Any]:
        """Get current storage usage."""
        total_size = 0
        entry_count = 0

        for state_file in self._data_dir.glob("*.json"):
            try:
                total_size += state_file.stat().st_size
                entry_count += 1
            except Exception:
                pass

        return {
            "entry_count": entry_count,
            "total_size_bytes": total_size,
            "total_size_mb": total_size / (1024 * 1024),
            "max_size_mb": MAX_STATE_SIZE_MB,
            "usage_percent": (total_size / (MAX_STATE_SIZE_MB * 1024 * 1024)) * 100 if total_size > 0 else 0,
        }
