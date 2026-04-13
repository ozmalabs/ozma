# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Connect Chat Adapter - First-class AI chat for Connect-authenticated users.

This adapter provides a seamless chat experience for Connect users with:
- No bot setup or identity mapping required
- Direct WebSocket communication through Connect relay
- Plan-based access control (free tier gets upgrade prompts)
- Integration with proactive alerts system

Protocol:
- Incoming: {type: 'agent_message', text: str, node_id?: str}
- Outgoing: {type: 'agent_chunk', text: str, done: bool}
"""

from __future__ import annotations

import asyncio
import json
import logging
from dataclasses import dataclass
from typing import Any, Callable, Optional
from datetime import datetime

from ..notifications import NotificationManager

log = logging.getLogger("ozma.messaging.connect_chat")


@dataclass
class ConnectUser:
    """Represents a Connect-authenticated user."""
    user_id: str
    username: str
    plan: str  # "free", "pro", "enterprise"
    active: bool


@dataclass
class ChatMessage:
    """Represents a chat message in the conversation."""
    id: str
    user_id: str
    text: str
    timestamp: datetime
    node_id: Optional[str] = None
    thread_id: Optional[str] = None


@dataclass
class ChatContext:
    """Context for processing a chat message."""
    user: ConnectUser
    message: ChatMessage
    response_stream: asyncio.Queue


class ConnectChatAdapter:
    """
    Connect Chat messaging adapter.
    
    Handles WebSocket communication with Connect relay for AI chat functionality.
    Integrates with notification system for proactive alerts.
    """
    
    def __init__(self, notification_manager: NotificationManager) -> None:
        self.notification_manager = notification_manager
        self._active_sessions: dict[str, ChatContext] = {}  # session_id -> context
        self._message_handlers: dict[str, Callable] = {
            "agent_message": self._handle_agent_message,
        }
        
    def list_sessions(self) -> list[dict]:
        """List active chat sessions."""
        return [
            {
                "user_id": ctx.user.user_id,
                "username": ctx.user.username,
                "plan": ctx.user.plan,
                "message_count": ctx.response_stream.qsize(),
            }
            for ctx in self._active_sessions.values()
        ]
        
    async def handle_websocket_message(self, message: dict, user: ConnectUser) -> None:
        """
        Handle incoming WebSocket message from Connect relay.
        
        Args:
            message: Parsed WebSocket message
            user: Authenticated Connect user
        """
        msg_type = message.get("type")
        if not msg_type:
            log.warning("Received WebSocket message without type")
            return
            
        handler = self._message_handlers.get(msg_type)
        if not handler:
            log.warning("Unknown WebSocket message type: %s", msg_type)
            return
            
        try:
            await handler(message, user)
        except Exception as e:
            log.error("Error handling WebSocket message: %s", e, exc_info=True)
            
    async def _handle_agent_message(self, message: dict, user: ConnectUser) -> None:
        """
        Handle agent_message from Connect user.
        
        Args:
            message: {type: 'agent_message', text: str, node_id?: str}
            user: Authenticated Connect user
        """
        text = message.get("text", "")
        node_id = message.get("node_id")
        thread_id = message.get("thread_id")
        
        if not text:
            log.warning("Received agent_message without text")
            return
            
        # Create chat message
        chat_msg = ChatMessage(
            id=f"msg_{int(datetime.now().timestamp() * 1000000)}",
            user_id=user.user_id,
            text=text,
            timestamp=datetime.now(),
            node_id=node_id,
            thread_id=thread_id,
        )
        
        # Create response stream
        response_stream = asyncio.Queue()
        
        # Create chat context
        context = ChatContext(
            user=user,
            message=chat_msg,
            response_stream=response_stream,
        )
        
        session_id = f"{user.user_id}_{chat_msg.id}"
        self._active_sessions[session_id] = context
        
        try:
            # Process the message based on user plan
            if user.plan == "free":
                await self._handle_free_user(context)
            else:
                await self._handle_paid_user(context)
        finally:
            # Clean up session
            self._active_sessions.pop(session_id, None)
            
    async def _handle_free_user(self, context: ChatContext) -> None:
        """
        Handle message from free tier user.
        
        Responds with upgrade prompt.
        """
        upgrade_prompt = (
            "👋 Hello! I'm Ozma AI, your smart home assistant.\n\n"
            "You're currently on the free plan. To unlock full AI chat capabilities, "
            "please upgrade to a paid plan.\n\n"
            "🔗 [Upgrade now](https://connect.ozma.io/upgrade)"
        )
        
        await self._send_response_chunk(context, upgrade_prompt, done=True)
        
    async def _handle_paid_user(self, context: ChatContext) -> None:
        """
        Handle message from paid user.
        
        Processes the message and streams response.
        """
        # TODO: Integrate with actual AI processing engine
        # This is a placeholder implementation
        
        user_msg = context.message
        response_text = f"I received your message: '{user_msg.text}'"
        
        if user_msg.node_id:
            response_text += f"\nContext: Node {user_msg.node_id}"
            
        response_text += "\n\nThis is a placeholder response. In a full implementation, "
        response_text += "this would be processed by the AI engine."
        
        # Stream the response in chunks
        chunk_size = 100
        for i in range(0, len(response_text), chunk_size):
            chunk = response_text[i:i + chunk_size]
            is_last = i + chunk_size >= len(response_text)
            await self._send_response_chunk(context, chunk, done=is_last)
            # Small delay to simulate streaming
            await asyncio.sleep(0.05)
            
    async def _send_response_chunk(self, context: ChatContext, text: str, done: bool = False) -> None:
        """
        Send a response chunk back through the WebSocket.
        
        Args:
            context: Chat context
            text: Response text chunk
            done: Whether this is the final chunk
        """
        chunk_msg = {
            "type": "agent_chunk",
            "text": text,
            "done": done,
            "timestamp": datetime.now().isoformat(),
        }
        
        # In a real implementation, this would send via WebSocket
        # For now, we'll put it on the response stream
        await context.response_stream.put(chunk_msg)
        
        log.debug("Sent response chunk to user %s: %s (done=%s)", 
                 context.user.user_id, text[:50], done)
                 
    async def send_proactive_alert(self, user_id: str, alert_text: str, 
                                 thread_id: Optional[str] = None) -> None:
        """
        Send a proactive alert to a Connect user.
        
        Integrates with notifications.py to push alerts as chat messages.
        
        Args:
            user_id: Connect user ID
            alert_text: Alert message text
            thread_id: Optional thread ID to associate with
        """
        alert_msg = {
            "type": "agent_chunk",
            "text": f"🚨 Alert: {alert_text}",
            "done": True,
            "proactive": True,
            "thread_id": thread_id or f"alert_{int(datetime.now().timestamp())}",
        }
        
        # TODO: Implement actual WebSocket sending mechanism
        log.info("Proactive alert for user %s: %s", user_id, alert_text)
        
        # Trigger notification event for potential other channels
        await self.notification_manager.on_event(
            "connect_chat.proactive_alert",
            {
                "user_id": user_id,
                "text": alert_text,
                "thread_id": alert_msg["thread_id"],
            }
        )
