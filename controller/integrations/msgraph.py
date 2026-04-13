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
            now = datetime.now()
            start_of_day = now.replace(hour=0, minute=0, second=0, microsecond=0)
            end_of_day = now.replace(hour=23, minute=59, second=59, microsecond=999999)
            
            # Query calendar events
            events = await self._client.me.calendar_view.get(
                start=start_of_day.isoformat(),
                end=end_of_day.isoformat()
            )
            
            if not events:
                return "No calendar events today"
                
            formatted = ["Today's calendar events:"]
            for event in events:
                subject = event.subject or "No subject"
                start_time = event.start.get("dateTime", "") if event.start else ""
                end_time = event.end.get("dateTime", "") if event.end else ""
                location = event.location.display_name if event.location else ""
                attendees = [attendee.email_address.name for attendee in event.attendees[:3]] if event.attendees else []
                
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
                '$top': 5,
                '$orderby': 'receivedDateTime DESC'
            }
            if search_query:
                query_params['$search'] = f'"{search_query}"'
            
            messages = await self._client.me.messages.get(
                **query_params
            )
            
            if not messages:
                return "No recent emails found"
                
            formatted = ["Recent emails:"]
            for msg in messages:
                subject = msg.subject or "No subject"
                sender = msg.sender.email_address.name if msg.sender and msg.sender.email_address else "Unknown"
                received = msg.received_date_time.strftime("%H:%M") if msg.received_date_time else ""
                preview = (msg.body.content[:100] + "...") if msg.body and msg.body.content else ""
                
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
            chats = await self._client.me.chats.get(top=3)
            
            if not chats:
                return "No recent chat conversations"
                
            formatted = ["Recent chat conversations:"]
            for chat in chats:
                chat_name = chat.topic or " ".join([p.display_name for p in chat.members[:2]]) if chat.members else "Unknown"
                formatted.append(f"- {chat_name}")
                
                # Get recent messages in this chat
                messages = await self._client.me.chats[chat.id].messages.get(top=3)
                for msg in messages:
                    sender = msg.sender.display_name if msg.sender else "Unknown"
                    content = msg.body.content if msg.body else ""
                    time = msg.created_date_time.strftime("%H:%M") if msg.created_date_time else ""
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
