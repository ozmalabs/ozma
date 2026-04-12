# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
User and zone management.

Users are the identity layer above controllers.  Each user owns a zone
(one or more controllers + their nodes + registered services).  Users
exist locally on the controller and can optionally sync to Ozma Connect.

Persistence: ``users.json`` next to main.py (same pattern as scenarios.json).
"""

from __future__ import annotations

import fcntl
import json
import logging
import os
import time
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from auth import hash_password, verify_password

log = logging.getLogger("ozma.users")


# ── Data models ──────────────────────────────────────────────────────────

@dataclass
class User:
    id: str                          # UUID, stable across local/Connect
    username: str                    # unique handle ("alice")
    display_name: str
    email: str = ""
    connect_account_id: str = ""     # links to Connect cloud account
    role: str = "owner"              # owner | member | guest
    password_hash: str = ""          # Argon2id
    created_at: float = 0.0
    last_seen: float = 0.0

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "username": self.username,
            "display_name": self.display_name,
            "email": self.email,
            "connect_account_id": self.connect_account_id,
            "role": self.role,
            "created_at": self.created_at,
            "last_seen": self.last_seen,
        }

    def to_storage(self) -> dict[str, Any]:
        """Full representation including password hash — for JSON persistence only."""
        d = self.to_dict()
        d["password_hash"] = self.password_hash
        return d

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> User:
        # Use safe defaults to avoid KeyError if JSON is corrupted.
        # The old code used d.get("display_name", d["username"]) which would
        # evaluate d["username"] even if display_name exists, causing KeyError
        # if username is missing. Now we use d.get() for all fields.
        return cls(
            id=d.get("id", ""),
            username=d.get("username", ""),
            display_name=d.get("display_name") or d.get("username") or "",
            email=d.get("email", ""),
            connect_account_id=d.get("connect_account_id", ""),
            role=d.get("role", "owner"),
            password_hash=d.get("password_hash", ""),
            created_at=d.get("created_at", 0.0),
            last_seen=d.get("last_seen", 0.0),
        )


@dataclass
class Zone:
    id: str
    owner_user_id: str
    name: str                        # "Alice's Home"
    subdomain: str = ""              # "alice" → alice.c.ozma.dev

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "owner_user_id": self.owner_user_id,
            "name": self.name,
            "subdomain": self.subdomain,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> Zone:
        return cls(
            id=d["id"],
            owner_user_id=d["owner_user_id"],
            name=d.get("name", ""),
            subdomain=d.get("subdomain", ""),
        )


# ── User manager ─────────────────────────────────────────────────────────

class UserManager:
    """Manages users with JSON file persistence.

    File format (``users.json``)::

        {
            "users": [ ... ],
            "zone": { ... }
        }
    """

    def __init__(self, path: Path) -> None:
        self._path = path
        self._lock_path = path.with_suffix(".json.lock")
        self._users: dict[str, User] = {}       # keyed by user.id
        self._zone: Zone | None = None
        self._load()

    # ── Persistence ──────────────────────────────────────────────────

    def _load(self) -> None:
        if not self._path.exists():
            return
        try:
            data = json.loads(self._path.read_text())
            for ud in data.get("users", []):
                try:
                    u = User.from_dict(ud)
                    # Skip invalid entries (missing id or username)
                    if not u.id or not u.username:
                        log.warning("Skipping invalid user entry: missing id or username")
                        continue
                    self._users[u.id] = u
                except (KeyError, TypeError) as e:
                    log.warning("Skipping malformed user entry: %s", e)
                    continue
            if data.get("zone"):
                self._zone = Zone.from_dict(data["zone"])
            log.info("Loaded %d user(s) from %s", len(self._users), self._path.name)
        except Exception as e:
            log.warning("Failed to load users: %s", e)

    def _save(self) -> None:
        """Save users to JSON file with file locking to prevent race conditions."""
        data: dict[str, Any] = {
            "users": [u.to_storage() for u in self._users.values()],
        }
        if self._zone:
            data["zone"] = self._zone.to_dict()
        tmp = self._path.with_suffix(".tmp")
        tmp.write_text(json.dumps(data, indent=2))
        os.chmod(tmp, 0o600)
        tmp.replace(self._path)

    def _acquire_lock(self, timeout: float = 30.0) -> int:
        """Acquire an exclusive lock on the users file.

        Uses fcntl.flock with non-blocking mode and retry.
        Returns the file descriptor on success.
        Raises TimeoutError if lock cannot be acquired within timeout.
        Raises PermissionError if lock file cannot be accessed.
        """
        lock_path = str(self._lock_path)
        lock_fd = None
        try:
            lock_fd = os.open(lock_path, os.O_CREAT | os.O_RDWR, 0o600)
        except PermissionError:
            raise PermissionError(f"Cannot access lock file: {lock_path}")
        except OSError as e:
            raise PermissionError(f"Cannot create lock file: {lock_path} - {e}")
        
        start_time = time.time()
        while True:
            try:
                fcntl.flock(lock_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                return lock_fd
            except (IOError, OSError):
                if time.time() - start_time >= timeout:
                    if lock_fd is not None:
                        try:
                            os.close(lock_fd)
                        except OSError:
                            pass
                    raise TimeoutError("Could not acquire lock within timeout")
                # Check for stale lock - if lock file was deleted by another process,
                # recreate it and continue trying to acquire the lock
                try:
                    if not os.path.exists(lock_path):
                        if lock_fd is not None:
                            try:
                                os.close(lock_fd)
                            except OSError:
                                pass
                        lock_fd = os.open(lock_path, os.O_CREAT | os.O_RDWR, 0o600)
                except (PermissionError, OSError):
                    pass
                time.sleep(0.05)  # Small delay before retry

    def _release_lock(self, lock_fd: int) -> None:
        """Release the lock and close the file descriptor."""
        try:
            fcntl.flock(lock_fd, fcntl.LOCK_UN)
        except (OSError, IOError):
            # Lock file may have been deleted or fd already closed
            pass
        try:
            os.close(lock_fd)
        except OSError:
            # fd already closed or invalid
            pass

    # ── User CRUD ────────────────────────────────────────────────────

    def create_user(self, username: str, display_name: str,
                    password: str = "", email: str = "",
                    role: str = "owner",
                    connect_account_id: str = "") -> User:
        """Create a new user.  Raises ValueError if username is taken."""
        # Acquire exclusive lock for atomic check-and-save
        lock_fd = self._acquire_lock()
        try:
            # Re-check for duplicate after acquiring lock (in case it was added)
            if any(u.username == username for u in self._users.values()):
                raise ValueError(f"Username already taken: {username}")
            user = User(
                id=str(uuid.uuid4()),
                username=username,
                display_name=display_name,
                email=email,
                connect_account_id=connect_account_id,
                role=role,
                password_hash=hash_password(password) if password else "",
                created_at=time.time(),
                last_seen=time.time(),
            )
            self._users[user.id] = user
            self._save()
            log.info("Created user %s (%s)", user.username, user.id)
            return user
        finally:
            self._release_lock(lock_fd)

    def create_user_with_hash(self, username: str, display_name: str,
                              password_hash: str, role: str = "owner") -> User:
        """Create a user with a pre-hashed password (for migration)."""
        # Acquire exclusive lock for atomic check-and-save
        lock_fd = self._acquire_lock()
        try:
            # Re-check for duplicate after acquiring lock (in case it was added)
            if any(u.username == username for u in self._users.values()):
                raise ValueError(f"Username already taken: {username}")
            user = User(
                id=str(uuid.uuid4()),
                username=username,
                display_name=display_name,
                role=role,
                password_hash=password_hash,
                created_at=time.time(),
                last_seen=time.time(),
            )
            self._users[user.id] = user
            self._save()
            log.info("Created user %s (%s) [migrated]", user.username, user.id)
            return user
        finally:
            self._release_lock(lock_fd)

    def get_user(self, user_id: str) -> User | None:
        return self._users.get(user_id)

    def get_by_username(self, username: str) -> User | None:
        for u in self._users.values():
            if u.username == username:
                return u
        return None

    def list_users(self) -> list[User]:
        return list(self._users.values())

    def update_user(self, user_id: str, **kwargs: Any) -> User | None:
        """Update user fields.  Accepts: display_name, email, role, password."""
        lock_fd = self._acquire_lock()
        try:
            user = self._users.get(user_id)
            if not user:
                return None
            if "display_name" in kwargs:
                user.display_name = kwargs["display_name"]
            if "email" in kwargs:
                user.email = kwargs["email"]
            if "role" in kwargs:
                user.role = kwargs["role"]
            if "password" in kwargs and kwargs["password"]:
                user.password_hash = hash_password(kwargs["password"])
            if "connect_account_id" in kwargs:
                user.connect_account_id = kwargs["connect_account_id"]
            self._save()
            return user
        finally:
            self._release_lock(lock_fd)

    def delete_user(self, user_id: str) -> bool:
        """Remove a user.  Returns True if the user existed."""
        lock_fd = self._acquire_lock()
        try:
            user = self._users.pop(user_id, None)
            if user:
                self._save()
                log.info("Deleted user %s (%s)", user.username, user.id)
                return True
            return False
        finally:
            self._release_lock(lock_fd)

    def authenticate(self, username: str, password: str) -> User | None:
        """Verify username + password.  Returns the User on success, None on failure."""
        lock_fd = self._acquire_lock()
        try:
            user = self.get_by_username(username)
            if not user or not user.password_hash:
                return None
            if verify_password(password, user.password_hash):
                user.last_seen = time.time()
                self._save()
                return user
            return None
        finally:
            self._release_lock(lock_fd)

    # ── Zone ─────────────────────────────────────────────────────────

    @property
    def zone(self) -> Zone | None:
        return self._zone

    def set_zone(self, zone: Zone) -> None:
        lock_fd = self._acquire_lock()
        try:
            self._zone = zone
            self._save()
        finally:
            self._release_lock(lock_fd)

    # ── Connect sync (stubs) ─────────────────────────────────────────

    async def sync_to_connect(self, connect_client: Any) -> None:
        """Push local user state to Ozma Connect.  Requires authenticated Connect client."""
        # TODO: implement when Connect user sync API is ready
        log.debug("sync_to_connect: not yet implemented")

    async def sync_from_connect(self, connect_client: Any) -> None:
        """Pull user updates from Ozma Connect."""
        # TODO: implement when Connect user sync API is ready
        log.debug("sync_from_connect: not yet implemented")

    # ── Helpers ───────────────────────────────────────────────────────

    @property
    def user_count(self) -> int:
        return len(self._users)

    def has_users(self) -> bool:
        return len(self._users) > 0
