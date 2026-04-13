# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Slack messaging channel adapter.

Supports:
- Slack Events API for receiving messages
- Web API for sending/responding to messages
- Slash command /ozma
- HMAC signature verification

Configuration:
- MESSAGING_SLACK_BOT_TOKEN
- MESSAGING_SLACK_SIGNING_SECRET
"""

import hashlib
import hmac
import json
import logging
import os
from typing import Optional

import httpx

log = logging.getLogger("ozma.messaging.slack")

class SlackChannel:
    def __init__(self):
        self.bot_token = os.environ.get("MESSAGING_SLACK_BOT_TOKEN")
        self.signing_secret = os.environ.get("MESSAGING_SLACK_SIGNING_SECRET")
        self._on_message_callback = None
        
        if self.bot_token:
            self.client = httpx.AsyncClient(
                base_url="https://slack.com/api/",
                headers={"Authorization": f"Bearer {self.bot_token}"}
            )

    async def start(self, on_message_callback):
        """Initialize the Slack channel."""
        self._on_message_callback = on_message_callback

    async def stop(self):
        """Clean up the Slack channel."""
        if hasattr(self, 'client'):
            await self.client.aclose()

    async def verify_signature(self, request_body: bytes, timestamp: str, signature: str) -> bool:
        """Verify the Slack request signature."""
        # Create the signed string
        sig_basestring = f"v0:{timestamp}:{request_body.decode()}"
        
        # Create our own signature
        my_signature = hmac.new(
            self.signing_secret.encode(),
            sig_basestring.encode(),
            hashlib.sha256
        ).hexdigest()
        
        # Compare signatures
        expected_signature = f"v0={my_signature}"
        return hmac.compare_digest(signature, expected_signature)

    async def handle_event(self, payload: dict):
        """Handle a Slack event payload."""
        if payload.get('type') == 'event_callback':
            event = payload.get('event', {})
            if event.get('type') == 'message' and not event.get('subtype'):
                # This is a regular message
                channel = event.get('channel')
                user = event.get('user')
                text = event.get('text', '')
                thread_ts = event.get('thread_ts') or event.get('ts')
                
                if self._on_message_callback:
                    await self._on_message_callback(
                        channel="slack",
                        user_id=user,
                        message=text,
                        thread_id=thread_ts,
                        metadata={'channel': channel}
                    )

    async def handle_command(self, payload: dict):
        """Handle a Slack slash command."""
        command = payload.get('command', '')
        if command == '/ozma':
            user_id = payload.get('user_id')
            channel_id = payload.get('channel_id')
            text = payload.get('text', '')
            
            if self._on_message_callback:
                await self._on_message_callback(
                    channel="slack",
                    user_id=user_id,
                    message=text,
                    thread_id=channel_id,  # Use channel as thread ID for commands
                    metadata={
                        'channel': channel_id,
                        'command': True
                    }
                )
            
            # Send immediate response
            return {
                "response_type": "ephemeral",
                "text": "Processing your request..."
            }

    async def send_message(self, channel: str, text: str, thread_ts: Optional[str] = None):
        """Send a message to a Slack channel."""
        if not self.bot_token:
            log.warning("Slack bot token not configured")
            return
            
        data = {
            "channel": channel,
            "text": text
        }
        
        if thread_ts:
            data["thread_ts"] = thread_ts
            
        try:
            response = await self.client.post("chat.postMessage", json=data)
            response.raise_for_status()
            return response.json()
        except Exception as e:
            log.error("Failed to send Slack message: %s", e)
            return None

    async def update_message(self, channel: str, ts: str, text: str):
        """Update an existing message in Slack."""
        if not self.bot_token:
            log.warning("Slack bot token not configured")
            return
            
        data = {
            "channel": channel,
            "ts": ts,
            "text": text
        }
        
        try:
            response = await self.client.post("chat.update", json=data)
            response.raise_for_status()
            return response.json()
        except Exception as e:
            log.error("Failed to update Slack message: %s", e)
            return None
