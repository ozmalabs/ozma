# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Slack messaging channel adapter.

Supports:
- Slack Events API for receiving messages
- Web API for sending/responding to messages
- Slash command /ozma
- HMAC signature verification
- OAuth-based authentication (preferred)
- Manual token configuration (backward compatibility)

Configuration (OAuth - preferred):
- Tokens provided via Connect OAuth flow
- Stored in Connect DB per controller

Configuration (manual - backward compatibility):
- MESSAGING_SLACK_BOT_TOKEN
- MESSAGING_SLACK_SIGNING_SECRET
"""

import asyncio
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
        # Manual configuration (backward compatibility)
        self.bot_token = os.environ.get("MESSAGING_SLACK_BOT_TOKEN")
        self.signing_secret = os.environ.get("MESSAGING_SLACK_SIGNING_SECRET")
        
        # OAuth configuration (preferred)
        self.oauth_token = None
        self.oauth_team_id = None
        
        self._on_message_callback = None
        self.client = None
        self._client_lock = asyncio.Lock()

    def _init_client(self):
        """Initialize the HTTP client with available token."""
        token = self.oauth_token or self.bot_token
        if token:
            # Validate token format (should start with xoxb- for bot tokens)
            if not token.startswith(('xoxb-', 'xoxp-')):
                log.warning("Slack token format appears invalid")
            
            self.client = httpx.AsyncClient(
                base_url="https://slack.com/api/",
                headers={"Authorization": f"Bearer {token}"}
            )

    def set_oauth_credentials(self, token: str, team_id: str):
        """Set OAuth credentials received from Connect."""
        # Validate token format (should start with xoxb- for bot tokens or xoxp- for user tokens)
        if not token.startswith(('xoxb-', 'xoxp-')):
            log.warning("Slack OAuth token format appears invalid: %s", token[:10] + "..." if len(token) > 10 else token)
        
        self.oauth_token = token
        self.oauth_team_id = team_id
        self._init_client()

    async def start(self, on_message_callback):
        """Initialize the Slack channel."""
        self._on_message_callback = on_message_callback
        # Initialize client if we have credentials
        if self.oauth_token or self.bot_token:
            async with self._client_lock:
                if not self.client:
                    self._init_client()

    async def stop(self):
        """Clean up the Slack channel."""
        if self.client:
            await self.client.aclose()
            self.client = None

    async def verify_signature(self, request_body: bytes, timestamp: str, signature: str) -> bool:
        """Verify the Slack request signature."""
        # Require signing secret for signature verification
        if not self.signing_secret:
            log.warning("Slack signing secret not configured")
            return False
            
        # Validate request body
        if not request_body:
            log.warning("Slack request body is empty")
            return False
            
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
        # Validate required parameters
        if not channel:
            log.warning("Slack channel parameter is required")
            return None
            
        if not text:
            log.warning("Slack text parameter is required")
            return None
            
        # Initialize client if not already done
        async with self._client_lock:
            if not self.client and (self.oauth_token or self.bot_token):
                self._init_client()
            
        if not self.client:
            log.warning("Slack client not initialized - missing token")
            return None
            
        data = {
            "channel": channel,
            "text": text
        }
        
        if thread_ts:
            data["thread_ts"] = thread_ts
            
        try:
            response = await self.client.post("chat.postMessage", json=data)
            response.raise_for_status()
            result = response.json()
            if not result.get('ok'):
                log.error("Slack API error: %s", result.get('error'))
                return None
            return result
        except httpx.HTTPStatusError as e:
            log.error("Slack HTTP error %s: %s", e.response.status_code, e.response.text)
            return None
        except Exception as e:
            log.error("Failed to send Slack message: %s", e)
            return None

    async def update_message(self, channel: str, ts: str, text: str):
        """Update an existing message in Slack."""
        # Validate required parameters
        if not channel:
            log.warning("Slack channel parameter is required")
            return None
            
        if not ts:
            log.warning("Slack ts parameter is required")
            return None
            
        if not text:
            log.warning("Slack text parameter is required")
            return None
            
        # Initialize client if not already done
        async with self._client_lock:
            if not self.client and (self.oauth_token or self.bot_token):
                self._init_client()
            
        if not self.client:
            log.warning("Slack client not initialized - missing token")
            return None
            
        data = {
            "channel": channel,
            "ts": ts,
            "text": text
        }
        
        try:
            response = await self.client.post("chat.update", json=data)
            response.raise_for_status()
            result = response.json()
            if not result.get('ok'):
                log.error("Slack API error: %s", result.get('error'))
                return None
            return result
        except httpx.HTTPStatusError as e:
            log.error("Slack HTTP error %s: %s", e.response.status_code, e.response.text)
            return None
        except Exception as e:
            log.error("Failed to update Slack message: %s", e)
            return None
