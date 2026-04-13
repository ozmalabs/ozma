# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Discord messaging channel adapter.

Supports:
- discord.py gateway for receiving messages
- Responding in same channel with user mentions
- !ozma prefix or @Ozma mention triggers

Configuration:
- MESSAGING_DISCORD_TOKEN
"""

import logging
import os
import re
from typing import Optional

try:
    import discord
    from discord.ext import commands
    DISCORD_AVAILABLE = True
except ImportError:
    DISCORD_AVAILABLE = False

log = logging.getLogger("ozma.messaging.discord")

class DiscordChannel:
    def __init__(self):
        self.token = os.environ.get("MESSAGING_DISCORD_TOKEN")
        self.client = None
        self._on_message_callback = None
        
        if DISCORD_AVAILABLE and self.token:
            intents = discord.Intents.default()
            intents.message_content = True
            self.client = commands.Bot(command_prefix='!', intents=intents)

    async def start(self, on_message_callback):
        """Start the Discord bot."""
        if not self.client:
            log.warning("Discord client not available or token not configured")
            return
            
        self._on_message_callback = on_message_callback
        
        @self.client.event
        async def on_ready():
            log.info(f"Discord bot logged in as {self.client.user}")
        
        @self.client.event
        async def on_message(message):
            # Don't respond to our own messages
            if message.author == self.client.user:
                return
                
            # Check for !ozma prefix or bot mention
            content = message.content
            bot_mentioned = self.client.user.mentioned_in(message)
            
            if content.startswith('!ozma') or bot_mentioned:
                # Strip the prefix/mention
                if content.startswith('!ozma'):
                    text = content[6:].strip()  # Remove "!ozma "
                elif bot_mentioned:
                    # Remove the mention
                    text = content.replace(f'<@{self.client.user.id}>', '').replace(f'<@!{self.client.user.id}>', '').strip()
                else:
                    text = content
                
                if self._on_message_callback:
                    await self._on_message_callback(
                        channel="discord",
                        user_id=str(message.author.id),
                        message=text,
                        thread_id=str(message.channel.id),
                        metadata={
                            'channel_id': str(message.channel.id),
                            'username': str(message.author),
                            'guild_id': str(message.guild.id) if message.guild else None
                        }
                    )
        
        # Start the bot
        if self.token:
            asyncio.create_task(self.client.start(self.token), name="discord-bot")

    async def stop(self):
        """Stop the Discord bot."""
        if self.client:
            await self.client.close()

    async def send_message(self, channel_id: str, text: str, mention_user: Optional[str] = None):
        """Send a message to a Discord channel."""
        if not self.client or not self.client.is_ready():
            log.warning("Discord client not ready")
            return
            
        try:
            channel = self.client.get_channel(int(channel_id))
            if not channel:
                log.warning("Channel %s not found", channel_id)
                return
                
            # Add user mention if provided
            if mention_user:
                text = f"<@{mention_user}> {text}"
                
            await channel.send(text)
        except Exception as e:
            log.error("Failed to send Discord message: %s", e)
