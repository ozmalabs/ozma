# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
GraphQL Subscription Event Dispatcher.

This module listens to AppState events and routes them to subscriber queues
based on event type filters. It runs as a background task that drains the
state.events queue and dispatches to GraphQL subscription subscribers.
"""

import asyncio
import logging
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from state import AppState

log = logging.getLogger("ozma.graphql.dispatcher")


class SubscriptionEventDispatcher:
    """
    Dispatches AppState events to GraphQL subscription subscriber queues.
    
    This runs as a background task that:
    1. Drains AppState.events queue
    2. Routes events to appropriate subscriber queues based on subscription type
    3. Handles cleanup when subscriptions are removed
    """
    
    def __init__(self, state: "AppState"):
        self._state = state
        self._running = False
        self._task: asyncio.Task | None = None
        self._subscription_context = None
    
    def set_subscription_context(self, context) -> None:
        """Set the subscription context for routing events."""
        self._subscription_context = context
    
    def start(self) -> None:
        """Start the event dispatcher."""
        if self._running:
            return
        self._running = True
        self._task = asyncio.create_task(self._run(), name="graphql-event-dispatcher")
    
    def stop(self) -> None:
        """Stop the event dispatcher."""
        self._running = False
        if self._task:
            self._task.cancel()
    
    async def _run(self) -> None:
        """Main loop that drains state.events and dispatches to subscribers."""
        while self._running:
            try:
                # Wait for next event
                event = await self._state.events.get()
                
                # Route to subscribers
                await self._dispatch_event(event)
                
            except asyncio.CancelledError:
                break
            except Exception as e:
                log.error("Error in event dispatcher: %s", e, exc_info=True)
    
    async def _dispatch_event(self, event: dict) -> None:
        """
        Dispatch an event to all subscriber queues that match its type.
        
        Event types are routed to subscription types:
        - node.online, node.offline, node.switched -> node subscriptions
        - scenario.activated -> scenario subscriptions
        - audio.levels -> audio subscriptions
        - alert.fired, alert.updated -> alert subscriptions
        """
        if not self._subscription_context:
            return
        
        event_type = event.get("type", "")
        
        # Map event types to subscription type filters
        event_type_to_subscription_type = {
            "node.online": "node",
            "node.offline": "node",
            "node.switched": "node",
            "scenario.activated": "scenario",
            "scenario.updated": "scenario",
            "scenario.created": "scenario",
            "scenario.deleted": "scenario",
            "audio.levels": "audio",
            "audio.node_online": "audio",
            "audio.node_offline": "audio",
            "alert.fired": "alert",
            "alert.updated": "alert",
            "alert.expired": "alert",
        }
        
        # Get matching subscription type
        subscription_type = event_type_to_subscription_type.get(event_type)
        if not subscription_type:
            return  # No subscriptions for this event type
        
        # Create a copy of subscribers to iterate safely
        # This prevents issues if a subscriber is removed during iteration
        async with asyncio.Lock():
            # Get all subscribers for this subscription type
            subscribers_copy = dict(self._subscription_context._subscribers)
        
        # Dispatch to matching subscribers
        for sub_id, queue in subscribers_copy.items():
            # Check if subscription is still active
            if sub_id not in self._subscription_context._subscribers:
                continue
            
            try:
                # Put event in queue (non-blocking, discard if full)
                queue.put_nowait(event)
            except asyncio.QueueFull:
                log.debug("Queue full for subscription %s, dropping event", sub_id)
            except Exception as e:
                log.debug("Error dispatching to subscription %s: %s", sub_id, e)
