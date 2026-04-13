# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Microsoft Graph integration for reading context into agent.

Provides read-only access to Outlook, Calendar, and Teams with user consent.
Supports meeting detection and calendar event context.
"""

from __future__ import annotations

import asyncio
import logging
from typing import Any, Dict, List, Optional
from datetime import datetime, timedelta

log = logging.getLogger("ozma.integrations.msgraph")

class MicrosoftGraphReader:
    """Read-only Microsoft Graph integration for agent context."""
    
    def __init__(self, graph_client: Any) -> None:
        """
        Initialize Microsoft Graph reader.
        
        Args:
            graph_client: Authenticated Microsoft Graph client with appropriate scopes
        """
        self._client = graph_client
        
    async def get_context(self, query: str) -> str:
        """
        Get context from Microsoft Graph based on query.
        
        Args:
            query: Query string like "today's meetings" or "emails about server"
            
        Returns:
            Formatted context string for agent consumption
        """
        if "meeting" in query or "calendar" in query:
            return await self._get_calendar_context(query)
        elif "email" in query or "mail" in query:
            return await self._get_email_context(query)
        elif "chat" in query or "teams" in query:
            return await self._get_chat_context(query)
        else:
            return "Please specify what type of context you want (meetings, emails, or chat)"
    
    async def _get_calendar_context(self, query: str) -> str:
        """Get calendar context."""
        try:
            # Get today's date range
            now = datetime.utcnow()
            start_of_day = now.replace(hour=0, minute=0, second=0, microsecond=0)
            end_of_day = now.replace(hour=23, minute=59, second=59, microsecond=999999)
            
            # Query calendar events
            events_result = await asyncio.get_event_loop().run_in_executor(
                None,
                lambda: self._client.me.calendar_view.get(
                    start_datetime=start_of_day.isoformat() + 'Z',
                    end_datetime=end_of_day.isoformat() + 'Z',
                    top=10
                )
            )
            events = events_result.value
            
            if not events:
                return "No calendar events today"
                
            formatted = ["Today's calendar events:"]
            for event in events:
                subject = getattr(event, 'subject', None) or "No subject"
                start_time = ""
                end_time = ""
                if hasattr(event, 'start') and event.start:
                    start_time = getattr(event.start, 'date_time', '') or ''
                if hasattr(event, 'end') and event.end:
                    end_time = getattr(event.end, 'date_time', '') or ''
                location = ""
                if hasattr(event, 'location') and event.location:
                    location = getattr(event.location, 'display_name', '') or ""
                attendees = []
                if hasattr(event, 'attendees') and event.attendees:
                    attendees = [getattr(getattr(attendee, 'email_address', ''), 'name', 'Unknown') 
                               for attendee in event.attendees[:3] if hasattr(attendee, 'email_address')]
                
                formatted.append(f"- {subject}")
                if start_time and end_time:
                    formatted.append(f"  Time: {start_time} - {end_time}")
                if location:
                    formatted.append(f"  Location: {location}")
                if attendees:
                    formatted.append(f"  Attendees: {', '.join(attendees)}")
                    
            return "\n".join(formatted)
        except Exception as e:
            log.error("Failed to fetch calendar events: %s", e)
            return f"Failed to fetch calendar events: {str(e)}"
    
    async def _get_email_context(self, query: str) -> str:
        """Get email context."""
        try:
            # Search for emails based on query keywords
            search_query = self._extract_search_terms(query)
            query_params = {
                'top': 5,
                'orderby': 'receivedDateTime DESC'
            }
            if search_query:
                query_params['search'] = search_query
            
            messages_result = await asyncio.get_event_loop().run_in_executor(
                None,
                lambda: self._client.me.messages.get(**query_params)
            )
            messages = messages_result.value
            
            if not messages:
                return "No recent emails found"
                
            formatted = ["Recent emails:"]
            for msg in messages:
                subject = getattr(msg, 'subject', None) or "No subject"
                sender = "Unknown"
                if hasattr(msg, 'sender') and msg.sender:
                    if hasattr(msg.sender, 'email_address') and msg.sender.email_address:
                        sender = getattr(msg.sender.email_address, 'name', 'Unknown') or "Unknown"
                received = ""
                if hasattr(msg, 'received_date_time') and msg.received_date_time:
                    try:
                        received = msg.received_date_time.strftime("%H:%M")
                    except:
                        received = ""
                preview = ""
                if hasattr(msg, 'body') and msg.body:
                    content = getattr(msg.body, 'content', '') or ''
                    if content:
                        preview = (content[:100] + "...") if len(content) > 100 else content
                
                formatted.append(f"- [{received}] {sender}: {subject}")
                if preview:
                    formatted.append(f"  {preview}")
                    
            return "\n".join(formatted)
        except Exception as e:
            log.error("Failed to fetch emails: %s", e)
            return f"Failed to fetch emails: {str(e)}"
    
    async def _get_chat_context(self, query: str) -> str:
        """Get chat context."""
        try:
            # Get recent chat messages
            chats_result = await asyncio.get_event_loop().run_in_executor(
                None,
                lambda: self._client.me.chats.get(top=3)
            )
            chats = chats_result.value
            
            if not chats:
                return "No recent chat conversations"
                
            formatted = ["Recent chat conversations:"]
            for chat in chats:
                chat_name = getattr(chat, 'topic', None) or "Unknown"
                if chat_name == "Unknown" and hasattr(chat, 'members') and chat.members:
                    member_names = [getattr(member, 'display_name', 'Unknown') for member in chat.members[:2]]
                    chat_name = " ".join(member_names) if member_names else "Unknown"
                formatted.append(f"- {chat_name}")
                
                # Get recent messages in this chat
                messages_result = await asyncio.get_event_loop().run_in_executor(
                    None,
                    lambda cid=chat.id: self._client.me.chats.by_chat_id(cid).messages.get(top=3)
                )
                messages = messages_result.value
                for msg in messages:
                    sender = "Unknown"
                    if hasattr(msg, 'sender') and msg.sender:
                        sender = getattr(msg.sender, 'display_name', 'Unknown') or "Unknown"
                    content = ""
                    if hasattr(msg, 'body') and msg.body:
                        content = getattr(msg.body, 'content', '') or ""
                    time = ""
                    if hasattr(msg, 'created_date_time') and msg.created_date_time:
                        try:
                            time = msg.created_date_time.strftime("%H:%M")
                        except:
                            time = ""
                    formatted.append(f"  [{time}] {sender}: {content}")
                    
            return "\n".join(formatted)
        except Exception as e:
            log.error("Failed to fetch chat messages: %s", e)
            return f"Failed to fetch chat messages: {str(e)}"
    
    def _extract_search_terms(self, query: str) -> Optional[str]:
        """Extract search terms from query."""
        import re
        match = re.search(r"about\s+([a-zA-Z0-9\s]+)", query)
        return match.group(1) if match else None
