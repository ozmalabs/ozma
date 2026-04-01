# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Device provisioning — stub for the Ozma Onboarding commercial module.

The full zero-touch provisioning pipeline (directory sync, bay management,
remote onboarding, shipping integration, SSO gateway) is available as
the Ozma Onboarding module — a commercial plugin from Ozma Labs.

This stub provides the interface so the open source controller can
integrate with the module when it's installed, and provides basic
provisioning bay status display for the open source ESP32 screen system.

For the full module: https://ozma.dev/onboarding
Install: symlink or clone into controller/plugins/ozma-onboarding/
"""

from __future__ import annotations

import asyncio
import json
import logging
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.provisioning")


@dataclass
class ProvisionBay:
    """A physical provisioning bay with an ozma node and optional screen."""
    id: str
    name: str
    node_id: str = ""
    screen_id: str = ""
    state: str = "available"
    current_user: str = ""
    device_serial: str = ""
    progress_pct: int = 0
    status_text: str = "Ready"

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id, "name": self.name,
            "node_id": self.node_id, "screen_id": self.screen_id,
            "state": self.state, "current_user": self.current_user,
            "device_serial": self.device_serial,
            "progress_pct": self.progress_pct,
            "status_text": self.status_text,
        }


@dataclass
class ProvisionJob:
    """A provisioning job — stub for API compatibility."""
    id: str
    state: str = "queued"
    progress_pct: int = 0
    status_text: str = "Queued"

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id, "state": self.state,
            "progress_pct": self.progress_pct,
            "status_text": self.status_text,
        }


class ProvisioningManager:
    """
    Stub provisioning manager.

    The open source version provides bay status display.
    Full provisioning requires the Ozma Onboarding module.
    """

    def __init__(self, state: Any = None, **kwargs: Any) -> None:
        self._state = state
        self._bays: dict[str, ProvisionBay] = {}
        self._config_path = Path(__file__).parent / "provisioning.json"
        self._load_config()

    def _load_config(self) -> None:
        if self._config_path.exists():
            try:
                data = json.loads(self._config_path.read_text())
                for b in data.get("bays", []):
                    bay = ProvisionBay(**{k: v for k, v in b.items()
                                         if k in ProvisionBay.__dataclass_fields__})
                    self._bays[bay.id] = bay
            except Exception:
                pass

    def _save_config(self) -> None:
        data = {"bays": [b.to_dict() for b in self._bays.values()]}
        self._config_path.write_text(json.dumps(data, indent=2))

    async def start(self) -> None:
        log.info("Provisioning stub active — install Ozma Onboarding for full provisioning")

    async def stop(self) -> None:
        self._save_config()

    def add_bay(self, bay_id: str, name: str, node_id: str,
                screen_id: str = "") -> ProvisionBay:
        bay = ProvisionBay(id=bay_id, name=name, node_id=node_id, screen_id=screen_id)
        self._bays[bay_id] = bay
        self._save_config()
        return bay

    def remove_bay(self, bay_id: str) -> bool:
        if bay_id in self._bays:
            del self._bays[bay_id]
            self._save_config()
            return True
        return False

    def list_bays(self) -> list[dict]:
        return [b.to_dict() for b in self._bays.values()]

    def list_profiles(self) -> list[dict]:
        return []  # Full module required

    def list_jobs(self, state: str = "") -> list[dict]:
        return []  # Full module required

    def get_job(self, job_id: str) -> ProvisionJob | None:
        return None  # Full module required

    async def create_job(self, user_data: dict, profile_id: str, **kwargs: Any) -> None:
        log.warning("Provisioning job creation requires the Ozma Onboarding module")
        return None

    async def mark_complete(self, job_id: str) -> bool:
        return False

    def add_profile(self, data: dict) -> Any:
        log.warning("Device profiles require the Ozma Onboarding module")
        return None

    async def sync_from_directory(self, directory_type: str, config: dict) -> list:
        log.warning("Directory sync requires the Ozma Onboarding module")
        return []

    def status(self) -> dict:
        return {
            "bays": self.list_bays(),
            "module": "stub",
            "message": "Install Ozma Onboarding for full provisioning",
        }
