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
import html
import websockets
from dataclasses import dataclass
from typing import Any, Callable, Optional
from datetime import datetime, timedelta

from ..notifications import NotificationManager

log = logging.getLogger("ozma.messaging.connect_chat")

# Maximum message length to prevent abuse
MAX_MESSAGE_LENGTH = 10000
# Maximum queue size for response streaming
MAX_RESPONSE_QUEUE_SIZE = 100
# WebSocket connection parameters
WEBSOCKET_RECONNECT_DELAY = 5  # seconds
WEBSOCKET_TIMEOUT = 30  # seconds
# Session timeout
SESSION_TIMEOUT = timedelta(hours=1)
# Rate limiting
MAX_MESSAGES_PER_MINUTE = 10


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
    last_activity: datetime = None


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
        self._message_counts: dict[str, list[datetime]] = {}  # user_id -> list of message timestamps
        
    async def start(self) -> None:
        """Start the WebSocket connection to Connect relay."""
        self._websocket_task = asyncio.create_task(self._websocket_handler())
        # Start session cleanup task
        asyncio.create_task(self._cleanup_sessions())
        
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
        try:
            self._websocket = await websockets.connect(
                self.relay_url,
                timeout=WEBSOCKET_TIMEOUT
            )
            self._connected = True
            self._reconnect_attempts = 0
            log.info("Connected to Connect relay")
        except Exception as e:
            log.error(f"Failed to connect to Connect relay: {e}")
            self._connected = False
            raise
            
    async def _listen_websocket(self) -> None:
        """Listen for incoming WebSocket messages."""
        if not self._websocket:
            return
            
        try:
            async for message in self._websocket:
                try:
                    data = json.loads(message)
                    await self._process_websocket_message(data)
                except json.JSONDecodeError as e:
                    log.warning(f"Invalid JSON message received: {e}")
                except Exception as e:
                    log.error(f"Error processing WebSocket message: {e}")
        except Exception as e:
            log.error(f"WebSocket connection error: {e}")
            self._connected = False
            
    async def _process_websocket_message(self, message: dict) -> None:
        """
        Process an incoming WebSocket message.
        
        Args:
            message: Parsed WebSocket message
        """
        # Log the message for debugging (sanitized)
        log.debug(f"Received WebSocket message: {str(message)[:200]}...")
        
        # Validate message structure
        if not isinstance(message, dict):
            log.warning("Received invalid message format")
            return
            
        # Extract and validate user information
        user_id = message.get("user_id")
        if not isinstance(user_id, str) or not user_id:
            log.warning("Received message without valid user_id")
            return
            
        username = message.get("username", "Unknown User")
        if not isinstance(username, str):
            log.warning("Invalid username format for user %s", user_id)
            return
            
        plan = message.get("plan", "free")
        if not isinstance(plan, str) or plan not in ["free", "pro", "enterprise"]:
            log.warning("Invalid plan for user %s: %s", user_id, plan)
            return
            
        active = message.get("active", True)
        if not isinstance(active, bool):
            log.warning("Invalid active flag for user %s", user_id)
            return
            
        user = ConnectUser(
            user_id=user_id,
            username=username,
            plan=plan,
            active=active
        )
        
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
        now = datetime.now()
        # Clean up expired sessions first
        expired_sessions = [
            session_id for session_id, ctx in self._active_sessions.items()
            if now - ctx.last_activity > SESSION_TIMEOUT
        ]
        for session_id in expired_sessions:
            self._active_sessions.pop(session_id, None)
            
        return [
            {
                "user_id": ctx.user.user_id,
                "username": ctx.user.username,
                "plan": ctx.user.plan,
                "message_count": ctx.response_stream.qsize(),
                "last_activity": ctx.last_activity.isoformat() if ctx.last_activity else None,
            }
            for ctx in self._active_sessions.values()
        ]
        
    async def _handle_agent_message(self, message: dict, user: ConnectUser) -> None:
        """
        Handle agent_message from Connect user.
        
        Args:
            message: {type: 'agent_message', text: str, node_id?: str}
            user: Authenticated Connect user
        """
        # Rate limiting check
        if not self._check_rate_limit(user.user_id):
            log.warning("Rate limit exceeded for user %s", user.user_id)
            await self._send_error_response(user, "Rate limit exceeded. Please wait before sending more messages.")
            return
            
        # Validate message content
        text = message.get("text", "")
        if not isinstance(text, str):
            log.warning("Received agent_message with invalid text from user %s", user.user_id)
            await self._send_error_response(user, "Invalid message format")
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
        
        if not text.strip():
            log.warning("Received agent_message without text from user %s", user.user_id)
            await self._send_error_response(user, "Message text is required")
            return
            
        node_id = message.get("node_id")
        thread_id = message.get("thread_id")
        
        # Validate optional fields
        if node_id is not None:
            if not isinstance(node_id, str) or not node_id:
                log.warning("Received agent_message with invalid node_id from user %s", user.user_id)
                await self._send_error_response(user, "Invalid node_id")
                return
            # Validate node_id format (alphanumeric, hyphens, underscores)
            if not re.match(r'^[a-zA-Z0-9\-_]+$', node_id):
                log.warning("Received agent_message with invalid node_id format from user %s", user.user_id)
                await self._send_error_response(user, "Invalid node_id format")
                return
            
        if thread_id is not None:
            if not isinstance(thread_id, str) or not thread_id:
                log.warning("Received agent_message with invalid thread_id from user %s", user.user_id)
                await self._send_error_response(user, "Invalid thread_id")
                return
            # Validate thread_id format
            if not re.match(r'^[a-zA-Z0-9\-_]+$', thread_id):
                log.warning("Received agent_message with invalid thread_id format from user %s", user.user_id)
                await self._send_error_response(user, "Invalid thread_id format")
                return
        
        # Generate secure message ID
        timestamp = int(datetime.now().timestamp() * 1000000)
        message_id = f"msg_{user.user_id[:8]}_{timestamp}"
        
        # Validate message ID format
        if not re.match(r'^[a-zA-Z0-9_\-]+$', message_id):
            log.warning("Generated invalid message ID for user %s", user.user_id)
            message_id = f"msg_{timestamp}"  # Fallback to timestamp-only ID
            
        # Create chat message
        chat_msg = ChatMessage(
            id=message_id,
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
            last_activity=datetime.now(),
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
            # Update last activity
            if session_id in self._active_sessions:
                self._active_sessions[session_id].last_activity = datetime.now()
            
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
            last_activity=datetime.now(),
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
            
        # More comprehensive pattern checks for potentially malicious content
        # Using html.escape to prevent XSS
        escaped_text = html.escape(text)
        if escaped_text != text:
            # If text was changed by escaping, it contained HTML entities
            return False
            
        # Check for common XSS patterns
        xss_patterns = [
            r'<\s*script',  # script tags
            r'javascript\s*:',  # JavaScript URLs
            r'on\w+\s*=',  # HTML event handlers
            r'<\s*iframe',  # iframe injection
            r'<\s*object',  # object injection
            r'<\s*embed',  # embed injection
            r'eval\s*\(',  # eval function
            r'document\.cookie',  # cookie access
            r'document\.write',  # document write
            r'\.innerHTML',  # innerHTML access
            r'expression\s*\(',  # CSS expressions
        ]
        
        text_lower = text.lower()
        for pattern in xss_patterns:
            if re.search(pattern, text_lower, re.IGNORECASE):
                return False
                
        return True
        
    def _check_rate_limit(self, user_id: str) -> bool:
        """
        Check if user has exceeded rate limit.
        
        Args:
            user_id: The user ID to check
            
        Returns:
            bool: True if user is within rate limit, False otherwise
        """
        now = datetime.now()
        one_minute_ago = now - timedelta(minutes=1)
        
        # Clean up old entries
        if user_id in self._message_counts:
            self._message_counts[user_id] = [
                timestamp for timestamp in self._message_counts[user_id]
                if timestamp > one_minute_ago
            ]
        else:
            self._message_counts[user_id] = []
            
        # Check if user is within rate limit
        if len(self._message_counts[user_id]) >= MAX_MESSAGES_PER_MINUTE:
            return False
            
        # Add current message to count
        self._message_counts[user_id].append(now)
        return True
        
    async def _cleanup_sessions(self) -> None:
        """
        Periodically clean up expired sessions.
        """
        while True:
            try:
                now = datetime.now()
                expired_sessions = [
                    session_id for session_id, ctx in self._active_sessions.items()
                    if now - ctx.last_activity > SESSION_TIMEOUT
                ]
                for session_id in expired_sessions:
                    self._active_sessions.pop(session_id, None)
                    log.debug(f"Cleaned up expired session: {session_id}")
                    
                # Clean up old rate limit entries
                one_minute_ago = now - timedelta(minutes=1)
                for user_id in list(self._message_counts.keys()):
                    self._message_counts[user_id] = [
                        timestamp for timestamp in self._message_counts[user_id]
                        if timestamp > one_minute_ago
                    ]
                    # Remove empty entries
                    if not self._message_counts[user_id]:
                        del self._message_counts[user_id]
                        
                await asyncio.sleep(60)  # Check every minute
            except asyncio.CancelledError:
                break
            except Exception as e:
                log.error(f"Error in session cleanup: {e}")
                 
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
            
        if thread_id is not None:
            if not isinstance(thread_id, str) or not thread_id:
                log.warning("Invalid thread_id for proactive alert")
                return
            # Validate thread_id format
            if not re.match(r'^[a-zA-Z0-9\-_]+$', thread_id):
                log.warning("Invalid thread_id format for proactive alert")
                thread_id = f"alert_{int(datetime.now().timestamp())}"  # Generate new one
            
        # Truncate alert text if too long
        if len(alert_text) > MAX_MESSAGE_LENGTH:
            alert_text = alert_text[:MAX_MESSAGE_LENGTH] + "..."
            
        # Generate thread ID if not provided
        if not thread_id:
            thread_id = f"alert_{int(datetime.now().timestamp())}"
            
        # Validate thread ID format
        if not re.match(r'^[a-zA-Z0-9_\-]+$', thread_id):
            log.warning("Invalid thread_id format for proactive alert")
            thread_id = f"alert_{int(datetime.now().timestamp())}"  # Generate new one
            
        alert_msg = {
            "type": "agent_chunk",
            "text": f"🚨 Alert: {alert_text}",
            "done": True,
            "proactive": True,
            "thread_id": thread_id,
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
                    "thread_id": thread_id,
                }
            )
        except Exception as e:
            log.error("Failed to send proactive alert notification for user %s: %s", user_id, e)
