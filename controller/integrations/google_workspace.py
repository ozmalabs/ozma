# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Google Workspace integration for reading context into agent.

Provides read-only access to Gmail and Calendar with user consent.
Supports meeting detection and calendar event context.
"""

from __future__ import annotations

import asyncio
import logging
from typing import Any, Dict, List, Optional
from datetime import datetime, timedelta

log = logging.getLogger("ozma.integrations.google")

class GoogleWorkspaceReader:
    """Read-only Google Workspace integration for agent context."""
    
    def __init__(self, gmail_client: Any, calendar_client: Any) -> None:
        """
        Initialize Google Workspace reader.
        
        Args:
            gmail_client: Authenticated Gmail client with appropriate scopes
            calendar_client: Authenticated Calendar client with appropriate scopes
        """
        self._gmail = gmail_client
        self._calendar = calendar_client
        
    async def get_context(self, query: str) -> str:
        """
        Get context from Google Workspace based on query.
        
        Args:
            query: Query string like "today's meetings" or "emails about server"
            
        Returns:
            Formatted context string for agent consumption
        """
        if "meeting" in query or "calendar" in query:
            return await self._get_calendar_context(query)
        elif "email" in query or "mail" in query:
            return await self._get_email_context(query)
        else:
            return "Please specify what type of context you want (meetings or emails)"
    
    async def _get_calendar_context(self, query: str) -> str:
        """Get calendar context."""
        try:
            # Get today's date range
            now = datetime.utcnow()
            start_of_day = now.replace(hour=0, minute=0, second=0, microsecond=0)
            end_of_day = now.replace(hour=23, minute=59, second=59, microsecond=999999)
            
            # Query calendar events
            events_result = await self._calendar.events().list(
                calendarId='primary',
                timeMin=start_of_day.isoformat() + 'Z',
                timeMax=end_of_day.isoformat() + 'Z',
                singleEvents=True,
                orderBy='startTime'
            ).execute()
            
            events = events_result.get('items', [])
            
            if not events:
                return "No calendar events today"
                
            formatted = ["Today's calendar events:"]
            for event in events:
                summary = event.get('summary', 'No title')
                start = event['start'].get('dateTime', event['start'].get('date'))
                end = event['end'].get('dateTime', event['end'].get('date'))
                location = event.get('location', '')
                attendees = [attendee['email'] for attendee in event.get('attendees', [])[:3]]
                
                formatted.append(f"- {summary}")
                if start and end:
                    # Parse time for better formatting
                    try:
                        start_dt = datetime.fromisoformat(start.replace('Z', '+00:00'))
                        end_dt = datetime.fromisoformat(end.replace('Z', '+00:00'))
                        time_range = f"{start_dt.strftime('%H:%M')} - {end_dt.strftime('%H:%M')}"
                        formatted.append(f"  Time: {time_range}")
                    except:
                        formatted.append(f"  Time: {start} - {end}")
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
            if search_query:
                query_param = f"subject:({search_query}) OR body:({search_query})"
            else:
                # Get recent emails
                query_param = "is:important OR is:unread"
                
            # Query Gmail
            results = await self._gmail.users().messages().list(
                userId='me',
                q=query_param,
                maxResults=5
            ).execute()
            
            messages = results.get('messages', [])
            
            if not messages:
                return "No matching emails found"
                
            formatted = ["Recent/relevant emails:"]
            for message in messages:
                msg = await self._gmail.users().messages().get(
                    userId='me',
                    id=message['id']
                ).execute()
                
                # Extract headers
                headers = msg['payload']['headers']
                subject = next((h['value'] for h in headers if h['name'] == 'Subject'), 'No subject')
                sender = next((h['value'] for h in headers if h['name'] == 'From'), 'Unknown')
                date = next((h['value'] for h in headers if h['name'] == 'Date'), '')
                
                # Extract snippet or body
                snippet = msg.get('snippet', '')
                
                # Format date
                try:
                    dt = datetime.strptime(date, '%a, %d %b %Y %H:%M:%S %z')
                    formatted_date = dt.strftime('%H:%M')
                except:
                    formatted_date = ''
                
                formatted.append(f"- [{formatted_date}] {sender}: {subject}")
                if snippet:
                    formatted.append(f"  {snippet}")
                    
            return "\n".join(formatted)
        except Exception as e:
            log.error("Failed to fetch emails: %s", e)
            return f"Failed to fetch emails: {str(e)}"
    
    def _extract_search_terms(self, query: str) -> Optional[str]:
        """Extract search terms from query."""
        import re
        match = re.search(r"about\s+([a-zA-Z0-9\s]+)", query)
        return match.group(1) if match else None
