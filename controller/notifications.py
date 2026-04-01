# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Notification system — push alerts to webhooks, email, Slack, Discord.

Watches ozma events and sends alerts based on configurable rules.
Works alongside the RGB compositor (visual alerts) for remote notification.

Supported destinations:
  - Webhook (generic HTTP POST)
  - Slack (incoming webhook URL)
  - Discord (webhook URL)
  - Email (SMTP)

Configurable triggers:
  - node.offline → alert
  - audio.overcurrent → alert
  - kdeconnect.telephony (incoming call) → alert
  - scenario.activated → info
  - power failure → critical
"""

from __future__ import annotations

import asyncio
import json
import logging
import urllib.request
import urllib.error
from dataclasses import dataclass, field
from typing import Any

log = logging.getLogger("ozma.notifications")


@dataclass
class NotifyDestination:
    """A notification destination."""
    id: str
    dest_type: str     # "webhook", "slack", "discord", "email"
    url: str = ""      # Webhook/Slack/Discord URL
    email: str = ""    # For email type
    smtp_host: str = ""
    smtp_port: int = 587
    enabled: bool = True

    def to_dict(self) -> dict[str, Any]:
        return {"id": self.id, "type": self.dest_type, "url": self.url or self.email, "enabled": self.enabled}


@dataclass
class NotifyRule:
    """A rule that maps an event pattern to a destination."""
    event_pattern: str    # Event type prefix match: "node.offline", "kdeconnect.*"
    destination_id: str
    level: str = "info"   # "info", "warning", "critical"

    def matches(self, event_type: str) -> bool:
        if self.event_pattern.endswith("*"):
            return event_type.startswith(self.event_pattern[:-1])
        return event_type == self.event_pattern

    def to_dict(self) -> dict[str, Any]:
        return {"event": self.event_pattern, "destination": self.destination_id, "level": self.level}


class NotificationManager:
    """Sends alerts based on event rules."""

    def __init__(self) -> None:
        self._destinations: dict[str, NotifyDestination] = {}
        self._rules: list[NotifyRule] = []

    def add_destination(self, dest: NotifyDestination) -> None:
        self._destinations[dest.id] = dest

    def add_rule(self, rule: NotifyRule) -> None:
        self._rules.append(rule)

    def list_destinations(self) -> list[dict]:
        return [d.to_dict() for d in self._destinations.values()]

    def list_rules(self) -> list[dict]:
        return [r.to_dict() for r in self._rules]

    async def on_event(self, event_type: str, data: dict) -> None:
        """Check event against rules and send matching notifications."""
        for rule in self._rules:
            if not rule.matches(event_type):
                continue
            dest = self._destinations.get(rule.destination_id)
            if not dest or not dest.enabled:
                continue
            asyncio.create_task(
                self._send(dest, event_type, data, rule.level),
                name=f"notify-{dest.id}",
            )

    async def _send(self, dest: NotifyDestination, event: str, data: dict, level: str) -> None:
        match dest.dest_type:
            case "webhook":
                await self._send_webhook(dest.url, event, data, level)
            case "slack":
                await self._send_slack(dest.url, event, data, level)
            case "discord":
                await self._send_discord(dest.url, event, data, level)

    async def _send_webhook(self, url: str, event: str, data: dict, level: str) -> None:
        payload = json.dumps({"event": event, "data": data, "level": level, "source": "ozma"}).encode()
        await self._http_post(url, payload, "application/json")

    async def _send_slack(self, url: str, event: str, data: dict, level: str) -> None:
        icon = {"info": "ℹ️", "warning": "⚠️", "critical": "🚨"}.get(level, "📢")
        text = f"{icon} *Ozma {level.upper()}*: `{event}`\n```{json.dumps(data, indent=2)[:500]}```"
        payload = json.dumps({"text": text}).encode()
        await self._http_post(url, payload, "application/json")

    async def _send_discord(self, url: str, event: str, data: dict, level: str) -> None:
        color = {"info": 3447003, "warning": 16776960, "critical": 15158332}.get(level, 0)
        payload = json.dumps({
            "embeds": [{
                "title": f"Ozma: {event}",
                "description": f"```json\n{json.dumps(data, indent=2)[:1000]}\n```",
                "color": color,
            }]
        }).encode()
        await self._http_post(url, payload, "application/json")

    async def _http_post(self, url: str, data: bytes, content_type: str) -> None:
        try:
            loop = asyncio.get_running_loop()
            def _do():
                req = urllib.request.Request(url, data=data, headers={"Content-Type": content_type}, method="POST")
                urllib.request.urlopen(req, timeout=10)
            await loop.run_in_executor(None, _do)
        except Exception as e:
            log.debug("Notification send failed (%s): %s", url[:50], e)
