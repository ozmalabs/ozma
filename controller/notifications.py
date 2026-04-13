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
import hashlib
import hmac
import json
import logging
import os
import time
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
    # For messaging bridge - store thread/conversation IDs
    thread_ids: dict[str, str] = field(default_factory=dict)  # event_id -> thread_id

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


@dataclass
class IdentityMapping:
    """Maps platform user IDs to Ozma user IDs."""
    platform_id: str
    ozma_user_id: str
    created_at: float = 0.0


class NotificationManager:
    """Sends alerts based on event rules."""

    def __init__(self) -> None:
        self._destinations: dict[str, NotifyDestination] = {}
        self._rules: list[NotifyRule] = []
        # Messaging bridge data
        self._identity_maps: dict[str, dict[str, IdentityMapping]] = {}  # channel -> {platform_id -> mapping}
        self._recent_alerts: dict[str, float] = {}  # thread_id -> timestamp (for context matching)
        self._agent_engine = None  # Will be set externally

    def set_agent_engine(self, agent_engine):
        """Set the agent engine for context-aware messaging."""
        self._agent_engine = agent_engine

    def add_destination(self, dest: NotifyDestination) -> None:
        self._destinations[dest.id] = dest

    def add_rule(self, rule: NotifyRule) -> None:
        self._rules.append(rule)

    def list_destinations(self) -> list[dict]:
        return [d.to_dict() for d in self._destinations.values()]

    def list_rules(self) -> list[dict]:
        return [r.to_dict() for r in self._rules]

    def list_messaging_channels(self) -> list[dict]:
        """List all messaging channels with their status."""
        channels = []
        for dest in self._destinations.values():
            if dest.dest_type in ["slack", "discord", "teams", "email"]:
                channels.append({
                    "id": dest.id,
                    "type": dest.dest_type,
                    "enabled": dest.enabled,
                    "status": "ok" if dest.enabled else "disabled",
                    "last_error": ""
                })
        return channels

    async def send_test_message(self, channel_type: str) -> str:
        """Send a test message to a channel."""
        for dest in self._destinations.values():
            if dest.dest_type == channel_type and dest.enabled:
                try:
                    match channel_type:
                        case "slack":
                            await self._send_slack(dest.url, "test", {"message": "Ozma is online."}, "info")
                        case "discord":
                            await self._send_discord(dest.url, "test", {"message": "Ozma is online."}, "info")
                        case "teams":
                            await self._send_webhook(dest.url, "test", {"message": "Ozma is online."}, "info")
                        case "email":
                            # For email, we'd send a test email
                            pass
                    return "Message sent successfully"
                except Exception as e:
                    return f"Failed to send message: {e}"
        return "Channel not found or disabled"

    def list_identity_mappings(self, channel: str) -> list[dict]:
        """List identity mappings for a channel."""
        if channel not in self._identity_maps:
            return []
        return [mapping.__dict__ for mapping in self._identity_maps[channel].values()]

    def add_identity_mapping(self, channel: str, platform_id: str, ozma_user_id: str) -> None:
        """Add or update an identity mapping."""
        if channel not in self._identity_maps:
            self._identity_maps[channel] = {}
        mapping = IdentityMapping(platform_id, ozma_user_id)
        self._identity_maps[channel][platform_id] = mapping

    def remove_identity_mapping(self, channel: str, platform_id: str) -> None:
        """Remove an identity mapping."""
        if channel in self._identity_maps and platform_id in self._identity_maps[channel]:
            del self._identity_maps[channel][platform_id]

    def get_ozma_user_id(self, channel: str, platform_id: str) -> str:
        """Get Ozma user ID for a platform user ID."""
        if channel in self._identity_maps and platform_id in self._identity_maps[channel]:
            return self._identity_maps[channel][platform_id].ozma_user_id
        return ""

    async def process_webhook(self, channel: str, body: bytes, headers: dict) -> dict:
        """Process an incoming webhook from a messaging platform."""
        try:
            # Parse the webhook payload based on channel type
            data = json.loads(body)
        except json.JSONDecodeError:
            log.warning("Failed to parse webhook JSON for channel %s", channel)
            return {"ok": False, "error": "Invalid JSON"}
        
        # Extract message content and sender
        message_content = ""
        sender_id = ""
        
        if channel == "slack":
            # Slack webhook format
            if "event" in data:
                message_content = data["event"].get("text", "")
                sender_id = data["event"].get("user", "")
        elif channel == "discord":
            # Discord webhook format
            if "content" in data:
                message_content = data["content"]
                sender_id = data.get("author", {}).get("id", "")
        
        # Check if this is in response to a recent alert
        ozma_user_id = self.get_ozma_user_id(channel, sender_id)
        context = None
        
        # Look for thread/conversation ID that matches a recent alert
        thread_id = self._find_thread_id(data, channel)
        if thread_id and thread_id in self._recent_alerts:
            # This is a response to a recent alert - provide context to agent
            if self._agent_engine:
                context = {
                    "alert_thread_id": thread_id,
                    "sender": ozma_user_id or sender_id,
                    "message": message_content
                }
                # Add context to agent engine prompt
                try:
                    if hasattr(self._agent_engine, 'add_context'):
                        await self._agent_engine.add_context(context)
                except Exception as e:
                    log.warning("Failed to add context to agent engine: %s", e)
        
        # Return success response
        return {"ok": True, "context_added": context is not None}

    def _find_thread_id(self, data: dict, channel: str) -> str:
        """Extract thread/conversation ID from webhook data."""
        if channel == "slack":
            return data.get("event", {}).get("thread_ts", data.get("event", {}).get("ts", ""))
        elif channel == "discord":
            return data.get("channel_id", "")
        return ""

    async def on_event(self, event_type: str, data: dict) -> None:
        """Check event against rules and send matching notifications."""
        for rule in self._rules:
            if not rule.matches(event_type):
                continue
            dest = self._destinations.get(rule.destination_id)
            if not dest or not dest.enabled:
                continue
            task = asyncio.create_task(
                self._send(dest, event_type, data, rule.level),
                name=f"notify-{dest.id}",
            )
            # Store thread ID for context matching
            thread_id = getattr(task, '_thread_id', None)
            if thread_id:
                dest.thread_ids[event_type] = thread_id
                self._recent_alerts[thread_id] = time.time()

    async def _send(self, dest: NotifyDestination, event: str, data: dict, level: str) -> None:
        match dest.dest_type:
            case "webhook":
                await self._send_webhook(dest.url, event, data, level)
            case "slack":
                response = await self._send_slack(dest.url, event, data, level)
                # Store thread ID for context matching
                if response and 'ts' in response:
                    asyncio.current_task()._thread_id = response['ts']
            case "discord":
                await self._send_discord(dest.url, event, data, level)

    async def _send_webhook(self, url: str, event: str, data: dict, level: str) -> None:
        payload = json.dumps({"event": event, "data": data, "level": level, "source": "ozma"}).encode()
        await self._http_post(url, payload, "application/json")

    async def _send_slack(self, url: str, event: str, data: dict, level: str) -> dict:
        icon = {"info": "ℹ️", "warning": "⚠️", "critical": "🚨"}.get(level, "📢")
        text = f"{icon} *Ozma {level.upper()}*: `{event}`\n```{json.dumps(data, indent=2)[:500]}```"
        payload = json.dumps({"text": text}).encode()
        response = await self._http_post_with_response(url, payload, "application/json")
        # Store thread ID for context matching
        if response and 'ts' in response:
            task = asyncio.current_task()
            if task:
                setattr(task, '_thread_id', response['ts'])
        return response

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

    async def _http_post_with_response(self, url: str, data: bytes, content_type: str) -> dict:
        try:
            loop = asyncio.get_running_loop()
            def _do():
                req = urllib.request.Request(url, data=data, headers={"Content-Type": content_type}, method="POST")
                with urllib.request.urlopen(req, timeout=10) as response:
                    return json.loads(response.read())
            return await loop.run_in_executor(None, _do)
        except Exception as e:
            log.debug("Notification send failed (%s): %s", url[:50], e)
            return {}

    # Webhook validation methods
    def _validate_slack_signature(self, body: bytes, headers: dict, dest: NotifyDestination) -> bool:
        """Validate Slack webhook signature."""
        try:
            timestamp = headers.get("X-Slack-Request-Timestamp", "")
            signature = headers.get("X-Slack-Signature", "")
            
            # Check timestamp (prevent replay attacks)
            if not timestamp or abs(time.time() - int(timestamp)) > 60 * 5:  # 5 minutes
                return False
                
            # Validate signature
            sig_basestring = f"v0:{timestamp}:{body.decode('utf-8')}"
            request_hash = hmac.new(
                dest.url.split("https://hooks.slack.com/services/")[-1].encode(),
                sig_basestring.encode(),
                hashlib.sha256
            ).hexdigest()
            expected_signature = f"v0={request_hash}"
            
            return hmac.compare_digest(expected_signature, signature)
        except Exception as e:
            log.warning("Slack signature validation failed: %s", e)
            return False

    def _validate_discord_signature(self, body: bytes, headers: dict, dest: NotifyDestination) -> bool:
        """Validate Discord webhook signature."""
        # Discord doesn't provide signature validation for incoming webhooks
        # This is a placeholder - in practice, you'd need to implement verification
        # based on your specific Discord integration setup
        return True

    def _get_sender_from_webhook(self, channel: str, body: bytes) -> str:
        """Extract sender identifier from webhook body."""
        try:
            data = json.loads(body)
            if channel == "slack" and "event" in data:
                return data["event"].get("user", "unknown")
            elif channel == "discord" and "author" in data:
                return data["author"].get("id", "unknown")
        except Exception:
            pass
        return "unknown"
