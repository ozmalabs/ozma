# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
GraphQL Subscriptions for real-time events over WebSocket.

This module provides async generators that yield typed objects in response
to events from AppState. Strawberry uses asyncio async generators for
subscriptions, which are automatically converted to WebSocket messages.

The existing /api/v1/ws WebSocket remains unchanged - this adds a separate
GraphQL subscription endpoint.
"""

import asyncio
import logging
from typing import AsyncGenerator, Callable
from dataclasses import dataclass

import strawberry
from strawberry.types import Info

from state import AppState, NodeInfo
from scenarios import ScenarioManager
from audio import AudioRouter

log = logging.getLogger("ozma.graphql.subscriptions")


# Rate limit for audio level updates (10Hz = 100ms interval)
AUDIO_LEVEL_INTERVAL = 0.1  # seconds


@strawberry.type
class Subscription:
    """
    GraphQL subscription root type.

    This class defines all subscription fields that can be queried
    over WebSocket connections. Each field is an async generator that
    yields typed objects as events occur in the system.

    Subscriptions are connected to AppState event queue and filter events
    by type, yielding properly typed objects to clients.

    Example query:
        subscription {
            nodeStateChanged
            scenarioActivated
            audioLevelUpdate
            alertFired
        }
    """

    @staticmethod
    @strawberry.subscription
    async def nodeStateChanged(
        info: Info,
    ) -> AsyncGenerator[dict, None]:
        """
        Subscribe to node state changes.

        Yields NodeType when a node comes online or goes offline.

        Events consumed:
            - node.online: node added to the system
            - node.offline: node removed from the system
            - node.switched: active node changed

        The active_node_id field in each yielded object indicates which
        node is currently active (None if no active node).
        """
        app_state: AppState = info.context["state"]
        active_node_id = app_state.active_node_id

        # Create a unique queue for this subscription to avoid conflicts
        # when multiple clients subscribe simultaneously
        subscription_id = id(asyncio.current_task())
        event_queue: asyncio.Queue = asyncio.Queue()

        # Register as a subscriber
        subscribers = info.context.setdefault("subscription_subscribers", {})
        subscribers[subscription_id] = event_queue

        try:
            # Send initial state snapshot
            nodes = app_state.nodes.copy()
            active = app_state.active_node_id
            yield {
                "type": "snapshot",
                "active_node_id": active,
                "nodes": {k: v.to_dict() for k, v in nodes.items()},
            }

            # Drain existing events in the queue first
            while True:
                try:
                    event = event_queue.get_nowait()
                    yield _format_node_state_event(event)
                except asyncio.QueueEmpty:
                    break

            # Yield new events as they arrive
            while True:
                event = await event_queue.get()
                yield _format_node_state_event(event)

        finally:
            # Cleanup subscription
            subscribers.pop(subscription_id, None)

    @staticmethod
    @strawberry.subscription
    async def scenarioActivated(
        info: Info,
    ) -> AsyncGenerator[dict, None]:
        """
        Subscribe to scenario activation events.

        Yields ScenarioType when a scenario is activated.

        Events consumed:
            - scenario.activated: scenario switching occurred

        The yielded object contains the scenario details including
        id, name, node_id, and configuration.
        """
        app_state: AppState = info.context["state"]
        scenario_mgr: ScenarioManager | None = info.context.get("scenario_manager")

        # Create a unique queue for this subscription
        subscription_id = id(asyncio.current_task())
        event_queue: asyncio.Queue = asyncio.Queue()

        subscribers = info.context.setdefault("subscription_subscribers", {})
        subscribers[subscription_id] = event_queue

        try:
            # Send initial state if scenario manager available
            if scenario_mgr:
                scenarios = scenario_mgr.list_scenarios()
                yield {
                    "type": "snapshot",
                    "scenarios": [s.to_dict() for s in scenarios],
                }

            # Yield new events
            while True:
                event = await event_queue.get()
                yield _format_scenario_event(event, scenario_mgr)

        finally:
            subscribers.pop(subscription_id, None)

    @staticmethod
    @strawberry.subscription
    async def audioLevelUpdate(
        info: Info,
    ) -> AsyncGenerator[dict, None]:
        """
        Subscribe to audio level updates.

        Yields per-node dB levels at approximately 10Hz (every 100ms).

        This subscription implements rate limiting to prevent overwhelming
        clients with too many updates. The audio router provides periodic
        level updates that are filtered and forwarded.

        Events consumed:
            - audio.levels: audio level measurements from the router

        Each yielded object contains:
            - node_id: the node the levels are from
            - levels: dict mapping channel names to dB values
            - timestamp: when the measurement was taken
        """
        app_state: AppState = info.context["state"]
        audio_router: AudioRouter | None = info.context.get("audio_router")

        if not audio_router:
            # If no audio router, yield periodically with empty data
            while True:
                yield {
                    "type": "update",
                    "timestamp": asyncio.get_event_loop().time(),
                    "levels": {},
                }
                await asyncio.sleep(AUDIO_LEVEL_INTERVAL)

        # Create a unique queue for this subscription
        subscription_id = id(asyncio.current_task())
        event_queue: asyncio.Queue = asyncio.Queue()

        subscribers = info.context.setdefault("subscription_subscribers", {})
        subscribers[subscription_id] = event_queue

        try:
            last_update_time = 0.0

            while True:
                event = await event_queue.get()
                current_time = asyncio.get_event_loop().time()

                # Rate limit: only yield if AUDIO_LEVEL_INTERVAL has passed
                if current_time - last_update_time >= AUDIO_LEVEL_INTERVAL:
                    yield _format_audio_event(event)
                    last_update_time = current_time
                else:
                    # Still process events but skip sending if too soon
                    pass

        finally:
            subscribers.pop(subscription_id, None)

    @staticmethod
    @strawberry.subscription
    async def alertFired(
        info: Info,
    ) -> AsyncGenerator[dict, None]:
        """
        Subscribe to alert events.

        Yields AlertType when an alert is fired in the system.

        Events consumed:
            - alert.fired: new alert raised
            - alert.updated: alert state changed (acknowledged/resolved)

        Each yielded object contains the full alert details including
        id, type, device_id, message, severity, timestamp, and source.
        """
        app_state: AppState = info.context["state"]

        # Create a unique queue for this subscription
        subscription_id = id(asyncio.current_task())
        event_queue: asyncio.Queue = asyncio.Queue()

        subscribers = info.context.setdefault("subscription_subscribers", {})
        subscribers[subscription_id] = event_queue

        try:
            # Send initial snapshot of pending alerts
            pending_alerts = getattr(app_state, "pending_alerts", [])
            if pending_alerts:
                yield {
                    "type": "snapshot",
                    "alerts": [a.to_dict() for a in pending_alerts],
                }

            # Yield new events
            while True:
                event = await event_queue.get()
                yield _format_alert_event(event)

        finally:
            subscribers.pop(subscription_id, None)


def _format_node_state_event(event: dict) -> dict:
    """Format a node state event for GraphQL subscription."""
    event_type = event.get("type", "unknown")
    result = {"type": event_type}

    if event_type == "node.online":
        result["node"] = event.get("node", {})
        result["active_node_id"] = None
    elif event_type == "node.offline":
        result["node_id"] = event.get("node_id", "")
        result["active_node_id"] = None
    elif event_type == "node.switched":
        result["node_id"] = event.get("node_id", "")
        result["active_node_id"] = event.get("node_id")
    elif event_type == "snapshot":
        result["nodes"] = event.get("nodes", {})
        result["active_node_id"] = event.get("active_node_id")
    else:
        result["data"] = event

    return result


def _format_scenario_event(event: dict, scenario_mgr: ScenarioManager | None) -> dict:
    """Format a scenario event for GraphQL subscription."""
    event_type = event.get("type", "unknown")
    result = {"type": event_type}

    if event_type == "scenario.activated":
        scenario_id = event.get("scenario_id")
        if scenario_id and scenario_mgr:
            scenario = scenario_mgr.get_scenario(scenario_id)
            if scenario:
                result["scenario"] = scenario.to_dict()
    elif event_type == "snapshot":
        if scenario_mgr:
            result["scenarios"] = [s.to_dict() for s in scenario_mgr.list_scenarios()]

    return result


def _format_audio_event(event: dict) -> dict:
    """Format an audio level event for GraphQL subscription."""
    event_type = event.get("type", "unknown")
    result = {"type": event_type, "timestamp": event.get("timestamp", 0)}

    if event_type == "audio.levels":
        result["node_id"] = event.get("node_id", "")
        result["levels"] = event.get("levels", {})

    return result


def _format_alert_event(event: dict) -> dict:
    """Format an alert event for GraphQL subscription."""
    event_type = event.get("type", "unknown")
    result = {"type": event_type}

    if event_type == "alert.fired":
        result["alert"] = event.get("alert", {})
    elif event_type == "alert.updated":
        result["alert_id"] = event.get("alert_id", "")
        result["updates"] = event.get("updates", {})

    return result
