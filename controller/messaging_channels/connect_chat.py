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
import re
import websockets
from dataclasses import dataclass
from typing import Any, Callable, Optional
from datetime import datetime

from ..notifications import NotificationManager

log = logging.getLogger("ozma.messaging.connect_chat")

# Maximum message length to prevent abuse
MAX_MESSAGE_LENGTH = 10000
# Maximum queue size for response streaming
MAX_RESPONSE_QUEUE_SIZE = 100
# WebSocket connection parameters
WEBSOCKET_RECONNECT_DELAY = 5  # seconds
WEBSOCKET_TIMEOUT = 30  # seconds


@dataclass
class ConnectUser:
    """Represents a Connect-authenticated user."""
    user_id: str
    username: str
    plan: str  # "free", "pro", "enterprise"
    active: bool

    def is_authenticated(self) -> bool:
        """Check if user is properly authenticated."""
        return bool(self.user_id) and self.active


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
    websocket: Any = None


class ConnectChatAdapter:
    """
    Connect Chat messaging adapter.
    
    Handles WebSocket communication with Connect relay for AI chat functionality.
    Integrates with notification system for proactive alerts.
    """
    
    def __init__(self, notification_manager: NotificationManager, relay_url: str = "ws://localhost:8765/connect") -> None:
        self.notification_manager = notification_manager
        self.relay_url = relay_url
        self._active_sessions: dict[str, ChatContext] = {}  # session_id -> context
        self._message_handlers: dict[str, Callable] = {
            "agent_message": self._handle_agent_message,
        }
        self._websocket_task: Optional[asyncio.Task] = None
        self._websocket: Optional[Any] = None
        self._connected = False
        self._reconnect_attempts = 0
        self._max_reconnect_attempts = 10
        
    async def start(self) -> None:
        """Start the WebSocket connection to Connect relay."""
        self._websocket_task = asyncio.create_task(self._websocket_handler())
        
    async def stop(self) -> None:
        """Stop the WebSocket connection."""
        if self._websocket_task:
            self._websocket_task.cancel()
            try:
                await self._websocket_task
            except asyncio.CancelledError:
                pass
        if self._websocket:
            await self._websocket.close()
            
    async def _websocket_handler(self) -> None:
        """Handle WebSocket connection lifecycle."""
        while True:
            try:
                await self._connect_websocket()
                await self._listen_websocket()
            except asyncio.CancelledError:
                log.info("WebSocket handler cancelled")
                break
            except Exception as e:
                log.error(f"WebSocket connection error: {e}")
                self._connected = False
                if self._reconnect_attempts < self._max_reconnect_attempts:
                    self._reconnect_attempts += 1
                    log.info(f"Reconnecting in {WEBSOCKET_RECONNECT_DELAY} seconds (attempt {self._reconnect_attempts})")
                    await asyncio.sleep(WEBSOCKET_RECONNECT_DELAY)
                else:
                    log.error("Max reconnect attempts reached, giving up")
                    break
                    
    async def _connect_websocket(self) -> None:
        """Connect to the Connect relay WebSocket."""
        log.info(f"Connecting to Connect relay at {self.relay_url}")
        self._websocket = await websockets.connect(
            self.relay_url,
            timeout=WEBSOCKET_TIMEOUT
        )
        self._connected = True
        self._reconnect_attempts = 0
        log.info("Connected to Connect relay")
        
    async def _listen_websocket(self) -> None:
        """Listen for incoming WebSocket messages."""
        if not self._websocket:
            return
            
        async for message in self._websocket:
            try:
                data = json.loads(message)
                await self._process_websocket_message(data)
            except json.JSONDecodeError as e:
                log.warning(f"Invalid JSON message received: {e}")
            except Exception as e:
                log.error(f"Error processing WebSocket message: {e}")
                
    async def _process_websocket_message(self, message: dict) -> None:
        """
        Process an incoming WebSocket message.
        
        Args:
            message: Parsed WebSocket message
        """
        # Extract user information from message (in real implementation, this would come from auth)
        # For now, we'll create a mock user for demonstration
        user = ConnectUser(
            user_id=message.get("user_id", "unknown"),
            username=message.get("username", "Unknown User"),
            plan=message.get("plan", "free"),
            active=True
        )
        
        # Validate message structure
        if not isinstance(message, dict):
            log.warning("Received invalid message format from user %s", user.user_id)
            return
            
        msg_type = message.get("type")
        if not msg_type:
            log.warning("Received WebSocket message without type from user %s", user.user_id)
            return
            
        # Validate message type
        if not isinstance(msg_type, str):
            log.warning("Received invalid message type from user %s", user.user_id)
            return
            
        handler = self._message_handlers.get(msg_type)
        if not handler:
            log.warning("Unknown WebSocket message type: %s from user %s", msg_type, user.user_id)
            return
            
        try:
            await handler(message, user)
        except asyncio.CancelledError:
            # Handle task cancellation gracefully
            log.debug("WebSocket message handling cancelled for user %s", user.user_id)
            raise
        except Exception as e:
            log.error("Error handling WebSocket message from user %s: %s", user.user_id, e, exc_info=True)
            
    async def _send_websocket_message(self, message: dict) -> None:
        """
        Send a message through the WebSocket connection.
        
        Args:
            message: Message to send
        """
        if not self._connected or not self._websocket:
            log.warning("Cannot send message, WebSocket not connected")
            return
            
        try:
            await self._websocket.send(json.dumps(message))
        except Exception as e:
            log.error(f"Failed to send WebSocket message: {e}")
            self._connected = False
            
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
        # Validate user authentication
        if not user.is_authenticated():
            log.warning("Received message from unauthenticated user")
            return
            
        # Validate message structure
        if not isinstance(message, dict):
            log.warning("Received invalid message format from user %s", user.user_id)
            return
            
        msg_type = message.get("type")
        if not msg_type:
            log.warning("Received WebSocket message without type from user %s", user.user_id)
            return
            
        # Validate message type
        if not isinstance(msg_type, str):
            log.warning("Received invalid message type from user %s", user.user_id)
            return
            
        handler = self._message_handlers.get(msg_type)
        if not handler:
            log.warning("Unknown WebSocket message type: %s from user %s", msg_type, user.user_id)
            return
            
        try:
            await handler(message, user)
        except asyncio.CancelledError:
            # Handle task cancellation gracefully
            log.debug("WebSocket message handling cancelled for user %s", user.user_id)
            raise
        except Exception as e:
            log.error("Error handling WebSocket message from user %s: %s", user.user_id, e, exc_info=True)
            
    async def _handle_agent_message(self, message: dict, user: ConnectUser) -> None:
        """
        Handle agent_message from Connect user.
        
        Args:
            message: {type: 'agent_message', text: str, node_id?: str}
            user: Authenticated Connect user
        """
        # Validate message content
        text = message.get("text", "")
        if not isinstance(text, str):
            log.warning("Received agent_message with invalid text from user %s", user.user_id)
            return
            
        # Check message length
        if len(text) > MAX_MESSAGE_LENGTH:
            log.warning("Received agent_message too long from user %s (length: %d)", user.user_id, len(text))
            await self._send_error_response(user, "Message too long")
            return
            
        # Validate text content (basic sanitization)
        if not self._is_valid_message_text(text):
            log.warning("Received agent_message with invalid content from user %s", user.user_id)
            await self._send_error_response(user, "Invalid message content")
            return
            
        node_id = message.get("node_id")
        thread_id = message.get("thread_id")
        
        # Validate optional fields
        if node_id is not None and not isinstance(node_id, str):
            log.warning("Received agent_message with invalid node_id from user %s", user.user_id)
            return
            
        if thread_id is not None and not isinstance(thread_id, str):
            log.warning("Received agent_message with invalid thread_id from user %s", user.user_id)
            return
        
        if not text.strip():
            log.warning("Received agent_message without text from user %s", user.user_id)
            await self._send_error_response(user, "Message text is required")
            return
            
        # Create chat message
        chat_msg = ChatMessage(
            id=f"msg_{int(datetime.now().timestamp() * 1000000)}",
            user_id=user.user_id,
            text=text.strip(),
            timestamp=datetime.now(),
            node_id=node_id,
            thread_id=thread_id,
        )
        
        # Create response stream with size limit
        response_stream = asyncio.Queue(maxsize=MAX_RESPONSE_QUEUE_SIZE)
        
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
            elif user.plan in ["pro", "enterprise"]:
                await self._handle_paid_user(context)
            else:
                log.warning("Unknown plan type for user %s: %s", user.user_id, user.plan)
                await self._handle_free_user(context)  # Default to free tier
        except asyncio.CancelledError:
            # Handle task cancellation gracefully
            log.debug("Message processing cancelled for user %s", user.user_id)
            raise
        except Exception as e:
            log.error("Error processing message for user %s: %s", user.user_id, e, exc_info=True)
            try:
                await self._send_error_response(user, "An error occurred processing your message")
            except Exception:
                log.error("Failed to send error response to user %s", user.user_id)
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
        # Validate text content
        if not isinstance(text, str):
            log.warning("Attempted to send non-string response chunk to user %s", context.user.user_id)
            return
            
        chunk_msg = {
            "type": "agent_chunk",
            "text": text,
            "done": done,
            "timestamp": datetime.now().isoformat(),
            "user_id": context.user.user_id,
            "thread_id": context.message.thread_id,
        }
        
        # Send via WebSocket
        await self._send_websocket_message(chunk_msg)
        
        log.debug("Sent response chunk to user %s: %s (done=%s)", 
                 context.user.user_id, text[:50] if text else "", done)
                 
    async def _send_error_response(self, user: ConnectUser, error_message: str) -> None:
        """
        Send an error response to the user.
        
        Args:
            user: The user to send the error to
            error_message: The error message to send
        """
        error_context = ChatContext(
            user=user,
            message=ChatMessage(
                id=f"error_{int(datetime.now().timestamp() * 1000000)}",
                user_id=user.user_id,
                text="",
                timestamp=datetime.now(),
            ),
            response_stream=asyncio.Queue(maxsize=MAX_RESPONSE_QUEUE_SIZE),
        )
        
        try:
            await self._send_response_chunk(error_context, f"❌ {error_message}", done=True)
        except Exception as e:
            log.error("Failed to send error response to user %s: %s", user.user_id, e)
                 
    def _is_valid_message_text(self, text: str) -> bool:
        """
        Validate message text for basic security checks.
        
        Args:
            text: The text to validate
            
        Returns:
            bool: True if text is valid, False otherwise
        """
        if not text:
            return True
            
        # Check for excessive whitespace
        if len(text) > 100 and text.count(' ') > len(text) * 0.5:
            return False
            
        # Basic pattern checks for potentially malicious content
        # Note: This is not comprehensive security validation
        malicious_patterns = [
            r'<script.*?>',  # Basic XSS patterns
            r'javascript:',  # JavaScript URLs
            r'on\w+\s*=',    # HTML event handlers
        ]
        
        text_lower = text.lower()
        for pattern in malicious_patterns:
            if re.search(pattern, text_lower, re.IGNORECASE):
                return False
                
        return True
                 
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
        # Validate inputs
        if not isinstance(user_id, str) or not user_id:
            log.warning("Invalid user_id for proactive alert")
            return
            
        if not isinstance(alert_text, str) or not alert_text.strip():
            log.warning("Invalid alert_text for proactive alert")
            return
            
        if thread_id is not None and not isinstance(thread_id, str):
            log.warning("Invalid thread_id for proactive alert")
            return
            
        # Truncate alert text if too long
        if len(alert_text) > MAX_MESSAGE_LENGTH:
            alert_text = alert_text[:MAX_MESSAGE_LENGTH] + "..."
            
        alert_msg = {
            "type": "agent_chunk",
            "text": f"🚨 Alert: {alert_text}",
            "done": True,
            "proactive": True,
            "thread_id": thread_id or f"alert_{int(datetime.now().timestamp())}",
            "user_id": user_id,
        }
        
        # Send via WebSocket
        await self._send_websocket_message(alert_msg)
        log.info("Proactive alert for user %s: %s", user_id, alert_text[:100])
        
        # Check if notification manager is available
        if not hasattr(self, 'notification_manager') or not self.notification_manager:
            log.warning("Notification manager not available for proactive alert")
            return
            
        try:
            # Trigger notification event for potential other channels
            await self.notification_manager.on_event(
                "connect_chat.proactive_alert",
                {
                    "user_id": user_id,
                    "text": alert_text,
                    "thread_id": alert_msg["thread_id"],
                }
            )
        except Exception as e:
            log.error("Failed to send proactive alert notification for user %s: %s", user_id, e)
