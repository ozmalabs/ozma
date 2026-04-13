from abc import ABC, abstractmethod
from dataclasses import dataclass
from datetime import datetime
from typing import AsyncIterator


@dataclass
class InboundMessage:
    """Represents an incoming message from a messaging platform."""
    channel: str
    thread_id: str  # platform-specific thread/conversation ID
    sender_platform_id: str  # email addr, user ID, phone, etc.
    text: str
    timestamp: datetime
    raw: dict  # original platform payload


class ChannelAdapter(ABC):
    """Base class for messaging channel adapters."""
    
    name: str  # 'slack', 'email', 'teams', etc.
    
    @abstractmethod
    async def start(self) -> None:
        """Begin polling or register webhook."""
        pass
    
    @abstractmethod
    async def stop(self) -> None:
        """Stop polling or unregister webhook."""
        pass
    
    @abstractmethod
    async def send(self, thread_id: str, text: str) -> None:
        """Send a message to a thread."""
        pass
    
    @abstractmethod
    async def send_chunks(self, thread_id: str, chunks: AsyncIterator[str]) -> None:
        """Send message chunks to a thread."""
        pass
