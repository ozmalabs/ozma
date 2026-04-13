# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Slack integration for reading context into agent.

Provides read-only access to Slack channels and groups with user consent.
Supports on-demand fetching of recent messages for context injection.
"""

from __future__ import annotations

import asyncio
import logging
import time
from typing import Any, Dict, List, Optional
from datetime import datetime, timedelta

log = logging.getLogger("ozma.integrations.slack")

class SlackReader:
    """Read-only Slack integration for agent context."""
    
    def __init__(self, slack_client: Any) -> None:
        """
        Initialize Slack reader.
        
        Args:
            slack_client: Authenticated Slack client with appropriate scopes
        """
        self._client = slack_client
        self._channels: Dict[str, str] = {}  # id -> name mapping
        self._last_cache_update = 0
        self._cache_ttl = 300  # 5 minutes
        
    async def _update_channel_cache(self) -> None:
        """Update the channel ID to name mapping cache."""
        if time.time() - self._last_cache_update < self._cache_ttl:
            return
            
        try:
            result = await self._client.conversations_list(
                types=["public_channel", "private_channel"],
                exclude_archived=True
            )
            
            self._channels = {
                channel["id"]: channel["name"] 
                for channel in result["channels"]
            }
            self._last_cache_update = time.time()
        except Exception as e:
            log.warning("Failed to update Slack channel cache: %s", e)
    
    async def get_context(self, query: str) -> str:
        """
        Get context from Slack based on query.
        
        Args:
            query: Query string like "last 5 messages in #infrastructure"
            
        Returns:
            Formatted context string for agent consumption
        """
        # Parse query for channel and time range
        channel_name = self._extract_channel_name(query)
        limit = self._extract_message_limit(query) or 10
        time_range = self._extract_time_range(query) or "today"
        
        if not channel_name:
            return "Please specify a channel name (e.g., '#general')"
            
        # Find channel ID
        await self._update_channel_cache()
        channel_id = None
        for cid, name in self._channels.items():
            if name.lower() == channel_name.lower().lstrip('#'):
                channel_id = cid
                break
                
        if not channel_id:
            return f"Channel '{channel_name}' not found"
            
        # Fetch messages
        try:
            messages = await self._fetch_messages(channel_id, limit, time_range)
            return self._format_context(messages, channel_name)
        except Exception as e:
            log.error("Failed to fetch Slack messages: %s", e)
            return f"Failed to fetch messages: {str(e)}"
    
    def _extract_channel_name(self, query: str) -> Optional[str]:
        """Extract channel name from query."""
        import re
        match = re.search(r"#(\w+)", query)
        return match.group(1) if match else None
    
    def _extract_message_limit(self, query: str) -> Optional[int]:
        """Extract message limit from query."""
        import re
        match = re.search(r"(\d+)\s+messages", query)
        return int(match.group(1)) if match else None
    
    def _extract_time_range(self, query: str) -> Optional[str]:
        """Extract time range from query."""
        if "today" in query:
            return "today"
        elif "yesterday" in query:
            return "yesterday"
        elif "week" in query:
            return "week"
        return None
    
    async def _fetch_messages(self, channel_id: str, limit: int, time_range: str) -> List[Dict]:
        """Fetch messages from a channel."""
        # Calculate time bounds
        oldest = None
        if time_range == "today":
            oldest = datetime.now().replace(hour=0, minute=0, second=0, microsecond=0).timestamp()
        elif time_range == "yesterday":
            yesterday = datetime.now() - timedelta(days=1)
            oldest = yesterday.replace(hour=0, minute=0, second=0, microsecond=0).timestamp()
        elif time_range == "week":
            week_ago = datetime.now() - timedelta(weeks=1)
            oldest = week_ago.timestamp()
            
        result = await self._client.conversations_history(
            channel=channel_id,
            limit=limit,
            oldest=oldest,
            inclusive=True
        )
        
        return result.get("messages", [])
    
    def _format_context(self, messages: List[Dict], channel_name: str) -> str:
        """Format messages as context string."""
        if not messages:
            return f"No recent messages found in #{channel_name}"
            
        formatted = [f"Recent messages from #{channel_name}:"]
        for msg in reversed(messages):  # Most recent first
            user = msg.get("user", "Unknown")
            text = msg.get("text", "").replace("\n", " ")
            ts = msg.get("ts", "")
            
            # Convert timestamp to readable format
            if ts:
                try:
                    dt = datetime.fromtimestamp(float(ts))
                    timestamp = dt.strftime("%H:%M")
                except:
                    timestamp = ""
            else:
                timestamp = ""
                
            formatted.append(f"[{timestamp}] {user}: {text}")
            
        return "\n".join(formatted)
