# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
User profiles with PIN lock.

Provides user profiles with PIN-based authentication and customizable app lists.

Features:
  - User profiles with PIN lock
  - Custom app icons per profile
  - Per-profile app list (subset of all scenarios)
"""

from __future__ import annotations

import asyncio
import hashlib
import hmac
import logging
import secrets
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.controller.gaming.profiles")


# ─── Constants ───────────────────────────────────────────────────────────────

PIN_LENGTH = 4
PIN_EXPIRY = 300  # 5 minutes
MAX_PIN_ATTEMPTS = 3


# ─── Profile Configuration ───────────────────────────────────────────────────

@dataclass
class UserProfile:
    """A user profile with PIN authentication."""
    user_id: str
    display_name: str
    pin_hash: str
    icon_data: str = ""  # Base64-encoded icon
    app_order: list[str] = field(default_factory=list)  # Ordered app IDs
    permissions: list[str] = field(default_factory=list)
    created_at: float = field(default_factory=time.time)
    last_login: float = field(default_factory=time.time)
    is_active: bool = True

    def to_dict(self) -> dict[str, Any]:
        return {
            "user_id": self.user_id,
            "display_name": self.display_name,
            "icon_data": self.icon_data,
            "app_order": self.app_order,
            "permissions": self.permissions,
            "created_at": self.created_at,
            "last_login": self.last_login,
            "is_active": self.is_active,
        }

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> "UserProfile":
        return cls(
            user_id=data.get("user_id", ""),
            display_name=data.get("display_name", ""),
            pin_hash=data.get("pin_hash", ""),
            icon_data=data.get("icon_data", ""),
            app_order=data.get("app_order", []),
            permissions=data.get("permissions", []),
            created_at=data.get("created_at", time.time()),
            last_login=data.get("last_login", time.time()),
            is_active=data.get("is_active", True),
        )


@dataclass
class PendingPin:
    """A pending PIN verification."""
    user_id: str
    pin_hash: str
    created_at: float = field(default_factory=time.time)
    attempts: int = 0


# ─── Profile Manager ─────────────────────────────────────────────────────────

class ProfileManager:
    """
    Manages user profiles with PIN authentication.

    Features:
      - Profile creation and management
      - PIN-based authentication
      - Per-profile app list customization
      - Permission-based access control
    """

    def __init__(self, data_dir: Path = Path("/var/lib/ozma/gaming/profiles")):
        self._data_dir = data_dir
        self._data_dir.mkdir(parents=True, exist_ok=True)

        self._profiles: dict[str, UserProfile] = {}
        self._pending_pins: dict[str, PendingPin] = {}
        self._active_sessions: dict[str, UserProfile] = {}

        # Load profiles from disk
        self._load_profiles()

    def _load_profiles(self) -> None:
        """Load profiles from disk."""
        profiles_file = self._data_dir / "profiles.json"
        if profiles_file.exists():
            try:
                import json
                data = json.loads(profiles_file.read_text())
                for profile_data in data.get("profiles", []):
                    profile = UserProfile.from_dict(profile_data)
                    self._profiles[profile.user_id] = profile
                log.info("Loaded %d user profiles", len(self._profiles))
            except Exception as e:
                log.error("Failed to load profiles: %s", e)

    def _save_profiles(self) -> None:
        """Save profiles to disk."""
        profiles_file = self._data_dir / "profiles.json"
        try:
            import json
            data = {
                "profiles": [p.to_dict() for p in self._profiles.values()],
                "last_save": time.time(),
            }
            profiles_file.write_text(json.dumps(data, indent=2))
        except Exception as e:
            log.error("Failed to save profiles: %s", e)

    # ─── Profile management ───────────────────────────────────────────────────

    def create_profile(
        self,
        user_id: str,
        display_name: str,
        pin: str,
        icon_data: str = "",
        permissions: list[str] | None = None,
    ) -> UserProfile | None:
        """Create a new user profile."""
        if user_id in self._profiles:
            return None

        if len(pin) != PIN_LENGTH or not pin.isdigit():
            return None

        # Hash the PIN
        pin_hash = self._hash_pin(pin)

        profile = UserProfile(
            user_id=user_id,
            display_name=display_name,
            pin_hash=pin_hash,
            icon_data=icon_data,
            permissions=permissions or ["stream"],
        )
        self._profiles[user_id] = profile
        self._save_profiles()

        log.info("Created profile for user %s", user_id)
        return profile

    def update_profile(
        self,
        user_id: str,
        display_name: str | None = None,
        icon_data: str | None = None,
        app_order: list[str] | None = None,
        permissions: list[str] | None = None,
    ) -> bool:
        """Update a user profile."""
        profile = self._profiles.get(user_id)
        if not profile:
            return False

        if display_name is not None:
            profile.display_name = display_name
        if icon_data is not None:
            profile.icon_data = icon_data
        if app_order is not None:
            profile.app_order = app_order
        if permissions is not None:
            profile.permissions = permissions

        profile.last_login = time.time()
        self._save_profiles()
        return True

    def delete_profile(self, user_id: str) -> bool:
        """Delete a user profile."""
        if user_id not in self._profiles:
            return False

        del self._profiles[user_id]
        self._save_profiles()
        return True

    def get_profile(self, user_id: str) -> UserProfile | None:
        """Get a user profile by ID."""
        return self._profiles.get(user_id)

    def get_all_profiles(self) -> list[UserProfile]:
        """Get all active profiles."""
        return [p for p in self._profiles.values() if p.is_active]

    # ─── PIN authentication ───────────────────────────────────────────────────

    def _hash_pin(self, pin: str) -> str:
        """Hash a PIN using SHA256."""
        return hashlib.sha256(pin.encode()).hexdigest()

    def generate_challenge(self, user_id: str, pin: str) -> str | None:
        """Generate a PIN challenge for verification."""
        profile = self._profiles.get(user_id)
        if not profile or not profile.is_active:
            return None

        if len(pin) != PIN_LENGTH:
            return None

        pin_hash = self._hash_pin(pin)
        if not hmac.compare_digest(pin_hash, profile.pin_hash):
            return None

        # Generate challenge
        challenge = secrets.token_hex(16)
        self._pending_pins[challenge] = PendingPin(
            user_id=user_id,
            pin_hash=pin_hash,
        )

        return challenge

    def verify_challenge(self, challenge: str) -> str | None:
        """Verify a PIN challenge and return user_id if valid."""
        pending = self._pending_pins.get(challenge)
        if not pending:
            return None

        # Remove the challenge
        del self._pending_pins[challenge]

        # Mark profile as active
        profile = self._profiles.get(pending.user_id)
        if profile:
            profile.last_login = time.time()
            self._save_profiles()

        return pending.user_id

    def start_session(self, user_id: str) -> bool:
        """Start a user session."""
        profile = self._profiles.get(user_id)
        if not profile or not profile.is_active:
            return False

        self._active_sessions[user_id] = profile
        return True

    def end_session(self, user_id: str) -> bool:
        """End a user session."""
        if user_id in self._active_sessions:
            del self._active_sessions[user_id]
            return True
        return False

    def get_active_session(self, user_id: str) -> UserProfile | None:
        """Get the active session for a user."""
        return self._active_sessions.get(user_id)

    # ─── App list customization ───────────────────────────────────────────────

    def get_profile_apps(self, user_id: str, all_apps: list[str]) -> list[str]:
        """Get the app list for a profile, ordered per profile preferences."""
        profile = self._profiles.get(user_id)
        if not profile:
            return all_apps

        # If no custom order, return all apps
        if not profile.app_order:
            return all_apps

        # Order apps according to profile preference
        ordered = [app for app in profile.app_order if app in all_apps]
        remaining = [app for app in all_apps if app not in profile.app_order]
        return ordered + remaining

    def set_app_order(self, user_id: str, app_order: list[str]) -> bool:
        """Set the app order for a profile."""
        profile = self._profiles.get(user_id)
        if not profile:
            return False

        profile.app_order = app_order
        profile.last_login = time.time()
        self._save_profiles()
        return True

    # ─── Permissions ──────────────────────────────────────────────────────────

    def check_permission(self, user_id: str, permission: str) -> bool:
        """Check if a user has a permission."""
        profile = self._profiles.get(user_id)
        if not profile:
            return False

        return permission in profile.permissions


# ─── PIN Session Manager ─────────────────────────────────────────────────────

class PINSessionManager:
    """
    Manages PIN verification sessions.

    Handles PIN entry, validation, and session creation.
    """

    def __init__(self, profile_mgr: ProfileManager):
        self._profiles = profile_mgr
        self._sessions: dict[str, dict[str, Any]] = {}

    def create_session(self, user_id: str) -> str | None:
        """Create a PIN verification session."""
        profile = self._profiles.get_profile(user_id)
        if not profile:
            return None

        session_id = secrets.token_hex(16)
        self._sessions[session_id] = {
            "user_id": user_id,
            "created_at": time.time(),
            "pin_attempts": 0,
        }
        return session_id

    def verify_pin(self, session_id: str, pin: str) -> bool:
        """Verify a PIN in a session."""
        session = self._sessions.get(session_id)
        if not session:
            return False

        profile = self._profiles.get_profile(session["user_id"])
        if not profile:
            return False

        # Check PIN
        pin_hash = hashlib.sha256(pin.encode()).hexdigest()
        if not hmac.compare_digest(pin_hash, profile.pin_hash):
            session["pin_attempts"] += 1
            if session["pin_attempts"] >= MAX_PIN_ATTEMPTS:
                del self._sessions[session_id]
            return False

        # Success - start the profile session
        self._profiles.start_session(session["user_id"])
        del self._sessions[session_id]
        return True

    def end_session(self, session_id: str) -> bool:
        """End a PIN session."""
        if session_id in self._sessions:
            del self._sessions[session_id]
            return True
        return False
