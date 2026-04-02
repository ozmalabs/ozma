# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
User and zone management.

Users are the identity layer above controllers.  Each user owns a zone
(one or more controllers + their nodes + registered services).  Users
exist locally on the controller and can optionally sync to Ozma Connect.

Persistence: ``users.json`` next to main.py (same pattern as scenarios.json).
"""

from __future__ import annotations

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
        return cls(
            id=d["id"],
            username=d["username"],
            display_name=d.get("display_name", d["username"]),
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
                u = User.from_dict(ud)
                self._users[u.id] = u
            if data.get("zone"):
                self._zone = Zone.from_dict(data["zone"])
            log.info("Loaded %d user(s) from %s", len(self._users), self._path.name)
        except Exception as e:
            log.warning("Failed to load users: %s", e)

    def _save(self) -> None:
        data: dict[str, Any] = {
            "users": [u.to_storage() for u in self._users.values()],
        }
        if self._zone:
            data["zone"] = self._zone.to_dict()
        tmp = self._path.with_suffix(".tmp")
        tmp.write_text(json.dumps(data, indent=2))
        os.chmod(tmp, 0o600)
        tmp.replace(self._path)

    # ── User CRUD ────────────────────────────────────────────────────

    def create_user(self, username: str, display_name: str,
                    password: str = "", email: str = "",
                    role: str = "owner",
                    connect_account_id: str = "") -> User:
        """Create a new user.  Raises ValueError if username is taken."""
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

    def create_user_with_hash(self, username: str, display_name: str,
                              password_hash: str, role: str = "owner") -> User:
        """Create a user with a pre-hashed password (for migration)."""
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

    def delete_user(self, user_id: str) -> bool:
        """Remove a user.  Returns True if the user existed."""
        user = self._users.pop(user_id, None)
        if user:
            self._save()
            log.info("Deleted user %s (%s)", user.username, user.id)
            return True
        return False

    def authenticate(self, username: str, password: str) -> User | None:
        """Verify username + password.  Returns the User on success, None on failure."""
        user = self.get_by_username(username)
        if not user or not user.password_hash:
            return None
        if verify_password(password, user.password_hash):
            user.last_seen = time.time()
            self._save()
            return user
        return None

    # ── Zone ─────────────────────────────────────────────────────────

    @property
    def zone(self) -> Zone | None:
        return self._zone

    def set_zone(self, zone: Zone) -> None:
        self._zone = zone
        self._save()

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
