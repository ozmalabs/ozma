# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Telegram messaging channel adapter.

Supports:
- python-telegram-bot in polling mode
- /start command for welcome message
- All other messages → bridge

Configuration:
- MESSAGING_TELEGRAM_TOKEN
"""

import asyncio
import logging
import os
from typing import Optional

try:
    from telegram import Update, BotCommand
    from telegram.ext import Application, CommandHandler, MessageHandler, filters
    TELEGRAM_AVAILABLE = True
except ImportError:
    TELEGRAM_AVAILABLE = False

log = logging.getLogger("ozma.messaging.telegram")

class TelegramChannel:
    def __init__(self):
        self.token = os.environ.get("MESSAGING_TELEGRAM_TOKEN")
        self.application = None
        self._on_message_callback = None
        
        if TELEGRAM_AVAILABLE and self.token:
            self.application = Application.builder().token(self.token).build()

    async def start(self, on_message_callback):
        """Start the Telegram bot."""
        if not self.application:
            log.warning("Telegram application not available or token not configured")
            return
            
        self._on_message_callback = on_message_callback
        
        # Add command handlers
        self.application.add_handler(CommandHandler("start", self._start_command))
        
        # Add message handler for all text messages
        self.application.add_handler(
            MessageHandler(filters.TEXT & ~filters.COMMAND, self._handle_message)
        )
        
        # Start polling
        async def _run_bot():
            await self.application.initialize()
            await self.application.start()
            await self.application.updater.start_polling()
            
        asyncio.create_task(_run_bot(), name="telegram-bot")

    async def stop(self):
        """Stop the Telegram bot."""
        if self.application:
            await self.application.updater.stop()
            await self.application.stop()
            await self.application.shutdown()

    async def _start_command(self, update: Update, context):
        """Handle the /start command."""
        user = update.effective_user
        welcome_text = (
            "Welcome to Ozma! 🚀\n\n"
            "I'm your AI assistant that can help with various tasks.\n"
            "Just send me a message and I'll do my best to help!\n\n"
            "For example, you can ask me to:\n"
            "- Explain something\n"
            "- Help with tasks\n"
            "- Answer questions\n"
        )
        
        await update.message.reply_text(welcome_text)

    async def _handle_message(self, update: Update, context):
        """Handle incoming text messages."""
        user = update.effective_user
        text = update.message.text
        
        if self._on_message_callback:
            await self._on_message_callback(
                channel="telegram",
                user_id=str(user.id),
                message=text,
                thread_id=str(update.message.chat_id),
                metadata={
                    'username': user.username,
                    'first_name': user.first_name,
                    'last_name': user.last_name,
                    'chat_type': update.message.chat.type
                }
            )

    async def send_message(self, chat_id: str, text: str):
        """Send a message to a Telegram chat."""
        if not self.application:
            log.warning("Telegram application not available")
            return
            
        try:
            await self.application.bot.send_message(chat_id=chat_id, text=text)
        except Exception as e:
            log.error("Failed to send Telegram message: %s", e)
