# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Apache Guacamole integration — stub for the Ozma Onboarding commercial module.

The full Guacamole integration (auto-deploy Docker stack, SSO configuration,
connection auto-sync, user management) is part of the Ozma Onboarding module.

This stub provides the interface so the open source controller can detect
and report Guacamole availability, and provides manual connection creation.

For the full module: https://ozma.dev/onboarding
"""

from __future__ import annotations

import asyncio
import json
import logging
from dataclasses import dataclass
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.guacamole")


class GuacamoleManager:
    """
    Stub Guacamole manager.

    Provides basic status reporting and manual connection management.
    Full auto-deploy, SSO, and directory sync require Ozma Onboarding.
    """

    def __init__(self, state: Any = None, **kwargs: Any) -> None:
        self._state = state
        self._config_path = Path(__file__).parent / "guacamole_config.json"
        self._base_url = ""
        self._connected = False
        self._load_config()

    def _load_config(self) -> None:
        if self._config_path.exists():
            try:
                data = json.loads(self._config_path.read_text())
                self._base_url = data.get("base_url", "")
            except Exception:
                pass

    async def start(self) -> None:
        if self._base_url:
            log.info("Guacamole URL configured: %s (stub mode — install Ozma Onboarding for full integration)", self._base_url)
        else:
            log.debug("Guacamole not configured")

    async def stop(self) -> None:
        pass

    async def deploy(self, config: dict) -> bool:
        log.warning("Guacamole auto-deploy requires the Ozma Onboarding module")
        return False

    async def teardown(self) -> bool:
        return False

    async def deployment_status(self) -> dict:
        return {"running": False, "module": "stub"}

    async def create_user(self, username: str, password: str,
                           connection_ids: list[str] | None = None) -> bool:
        log.warning("Guacamole user management requires the Ozma Onboarding module")
        return False

    async def list_users(self) -> list[dict]:
        return []

    async def list_connections(self) -> list[dict]:
        return []

    def status(self) -> dict:
        return {
            "connected": False,
            "base_url": self._base_url,
            "module": "stub",
            "message": "Install Ozma Onboarding for full Guacamole integration",
        }
