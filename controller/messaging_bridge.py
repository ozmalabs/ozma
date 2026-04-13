import asyncio
import json
import os
from pathlib import Path
from typing import Dict, AsyncIterator

from controller.messaging_channels.base import ChannelAdapter, InboundMessage


class MessagingBridge:
    """Central messaging bridge that routes messages between platforms and Ozma."""
    
    def __init__(self, state: "AppState"):  # type: ignore[name-defined]
        self.state = state
        self.channels: Dict[str, ChannelAdapter] = {}
        self.identity_map_path = Path(os.environ.get("OZMA_DATA_DIR", ".")) / "messaging_identity_map.json"
        self._load_identity_map()
    
    def _load_identity_map(self) -> None:
        """Load the identity map from JSON file."""
        try:
            with open(self.identity_map_path, 'r') as f:
                self.identity_map = json.load(f)
        except FileNotFoundError:
            self.identity_map = {}
    
    def _save_identity_map(self) -> None:
        """Save the identity map to JSON file."""
        with open(self.identity_map_path, 'w') as f:
            json.dump(self.identity_map, f, indent=2)
    
    def register_channel(self, adapter: ChannelAdapter) -> None:
        """Register a channel adapter."""
        self.channels[adapter.name] = adapter
    
    async def start_all(self) -> None:
        """Start all registered channel adapters."""
        await asyncio.gather(*(adapter.start() for adapter in self.channels.values()))
    
    async def stop_all(self) -> None:
        """Stop all registered channel adapters."""
        await asyncio.gather(*(adapter.stop() for adapter in self.channels.values()))
    
    async def on_message(self, msg: InboundMessage) -> None:
        """Handle an incoming message."""
        # Look up Ozma user from sender_platform_id + channel in identity_map
        ozma_user_id = self.identity_map.get(msg.channel, {}).get(msg.sender_platform_id)
        
        # Get the channel adapter
        adapter = self.channels.get(msg.channel)
        if not adapter:
            # This shouldn't happen but let's be safe
            return
        
        if not ozma_user_id:
            # If not found: send rejection message via adapter
            await adapter.send(msg.thread_id, "Sorry, I don't recognize your account. Please contact an administrator to link your account.")
            return
        
        # If MESSAGING_OPEN_MODE=1: map to owner user (single-user / home setup)
        messaging_open_mode = os.environ.get("MESSAGING_OPEN_MODE", "0")
        if messaging_open_mode == "1":
            # In open mode, we map to the owner user
            # This would typically be configured somewhere in the state
            ozma_user_id = self._get_owner_user_id()  # This is a placeholder method
        
        # Route to agent_engine.handle_message(user_id, text, context)
        # For now, we'll simulate this with a placeholder
        response_chunks = self._handle_message(ozma_user_id, msg.text)
        
        # Stream response chunks back via adapter.send_chunks()
        await adapter.send_chunks(msg.thread_id, response_chunks)
    
    def _get_owner_user_id(self) -> str:
        """Get the owner user ID for open mode."""
        # This is a placeholder implementation
        return "owner"
    
    def _handle_message(self, user_id: str, text: str) -> AsyncIterator[str]:
        """Placeholder for agent_engine.handle_message - would be replaced with actual implementation."""
        async def chunk_generator():
            # This is a mock implementation
            yield "I received your message: "
            yield text
            yield "\nThank you for contacting Ozma!"
        return chunk_generator()
