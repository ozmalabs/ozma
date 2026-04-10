# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
GraphQL Subscriptions for real-time events over WebSocket.

This module provides async generators that yield typed objects in response
to events from AppState. Strawberry uses asyncio async generators for
subscriptions, which are automatically converted to WebSocket messages.

The existing /api/v1/events WebSocket remains unchanged - this adds a separate
GraphQL subscription endpoint.
"""

import asyncio
import logging
import time
from typing import AsyncGenerator

import strawberry
from strawberry.types import Info

from state import AppState, NodeInfo
from scenarios import ScenarioManager
from audio import AudioRouter

log = logging.getLogger("ozma.graphql.subscriptions")


# Rate limit for audio level updates (10Hz = 100ms interval)
AUDIO_LEVEL_INTERVAL = 0.1  # seconds


@strawberry.type
class NodeType:
    """GraphQL type for a node in the KVMA system."""

    id: str
    host: str
    port: int
    role: str
    hw: str
    fw_version: str
    proto_version: int
    capabilities: list[str]
    machine_class: str
    last_seen: float
    display_outputs: list[dict]
    vnc_host: str | None
    vnc_port: int | None
    stream_port: int | None
    stream_path: str | None
    api_port: int | None
    audio_type: str | None
    audio_sink: str | None
    audio_vban_port: int | None
    mic_vban_port: int | None
    capture_device: str | None
    camera_streams: list[dict]
    frigate_host: str | None
    frigate_port: int | None
    owner_user_id: str
    owner_id: str
    shared_with: list[str]
    seat_count: int
    seat_config: dict
    parent_node_id: str
    sunshine_port: int | None
    active: bool

    @staticmethod
    def from_node(node: NodeInfo, active: bool = False) -> "NodeType":
        """Convert internal NodeInfo to GraphQL NodeType."""
        return NodeType(
            id=node.id,
            host=node.host,
            port=node.port,
            role=node.role,
            hw=node.hw,
            fw_version=node.fw_version,
            proto_version=node.proto_version,
            capabilities=node.capabilities,
            machine_class=node.machine_class,
            last_seen=node.last_seen,
            display_outputs=node.display_outputs,
            vnc_host=node.vnc_host,
            vnc_port=node.vnc_port,
            stream_port=node.stream_port,
            stream_path=node.stream_path,
            api_port=node.api_port,
            audio_type=node.audio_type,
            audio_sink=node.audio_sink,
            audio_vban_port=node.audio_vban_port,
            mic_vban_port=node.mic_vban_port,
            capture_device=node.capture_device,
            camera_streams=node.camera_streams,
            frigate_host=node.frigate_host,
            frigate_port=node.frigate_port,
            owner_user_id=node.owner_user_id,
            owner_id=node.owner_id,
            shared_with=node.shared_with,
            seat_count=node.seat_count,
            seat_config=node.seat_config,
            parent_node_id=node.parent_node_id,
            sunshine_port=node.sunshine_port,
            active=active,
        )


@strawberry.type
class ScenarioType:
    """GraphQL type for a scenario."""

    id: str
    name: str
    node_id: str | None
    color: str
    index: int
    config: dict

    @staticmethod
    def from_scenario(scenario) -> "ScenarioType":
        """Convert internal Scenario to GraphQL ScenarioType."""
        return ScenarioType(
            id=scenario.id,
            name=scenario.name,
            node_id=scenario.node_id,
            color=scenario.color,
            index=scenario.index,
            config=scenario.config,
        )


@strawberry.type
class AlertType:
    """GraphQL type for an alert."""

    id: str
    kind: str
    title: str
    body: str
    camera: str | None
    person: str | None
    severity: str
    state: str
    created_at: float
    timeout_s: float
    camera_id: str | None

    @staticmethod
    def from_alert(alert) -> "AlertType":
        """Convert internal alert to GraphQL AlertType."""
        return AlertType(
            id=alert.id,
            kind=alert.kind,
            title=alert.title,
            body=alert.body,
            camera=alert.camera,
            person=alert.person,
            severity=alert.severity,
            state=alert.state,
            created_at=alert.created_at,
            timeout_s=alert.timeout_s,
            camera_id=alert.camera_id,
        )


@strawberry.type
class AudioLevelType:
    """GraphQL type for audio level data for a single node."""

    node_id: str
    levels: dict[str, float] = strawberry.field(
        description="Mapping of channel names to dB values"
    )
    timestamp: float


@strawberry.type
class SnapshotType:
    """GraphQL type for system snapshot."""

    nodes: list[NodeType]
    active_node_id: str | None


# Global subscription tracking
class _SubscriptionRegistry:
    """Thread-safe registry for subscription queues."""

    def __init__(self):
        self._queues: dict[str, asyncio.Queue] = {}
        self._event_types: dict[str, str] = {}  # subscription_id -> event_type_filter
        self._lock = asyncio.Lock()

    async def register(
        self, subscription_id: str, queue: asyncio.Queue, event_type_filter: str
    ) -> None:
        """Register a subscription."""
        async with self._lock:
            self._queues[subscription_id] = queue
            self._event_types[subscription_id] = event_type_filter

    async def unregister(self, subscription_id: str) -> None:
        """Unregister a subscription."""
        async with self._lock:
            self._queues.pop(subscription_id, None)
            self._event_types.pop(subscription_id, None)

    async def get_queue(self, subscription_id: str) -> asyncio.Queue | None:
        """Get a subscription queue by ID."""
        async with self._lock:
            return self._queues.get(subscription_id)

    async def get_event_type_filter(self, subscription_id: str) -> str | None:
        """Get the event type filter for a subscription."""
        async with self._lock:
            return self._event_types.get(subscription_id)

    async def get_all_event_types(self) -> dict[str, str]:
        """Get all event type filters."""
        async with self._lock:
            return dict(self._event_types)

    async def get_all_queues(self) -> dict[str, asyncio.Queue]:
        """Get all queues."""
        async with self._lock:
            return dict(self._queues)


# Global registry instance
_subscription_registry = _SubscriptionRegistry()


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
                result["scenario"] = ScenarioType.from_scenario(scenario).__dict__
    elif event_type == "snapshot":
        if scenario_mgr:
            result["scenarios"] = [
                ScenarioType.from_scenario(s).__dict__
                for s in scenario_mgr.list_scenarios()
            ]

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


def _matches_event_type(event: dict, event_type_filter: str) -> bool:
    """Check if an event matches a subscription's event type filter."""
    event_type = event.get("type", "")

    # Map subscription type filters to matching event types
    filter_map = {
        "node": [
            "node.online",
            "node.offline",
            "node.switched",
        ],
        "scenario": [
            "scenario.activated",
            "scenario.updated",
            "scenario.created",
            "scenario.deleted",
            "scenario.transitioning",
        ],
        "audio": [
            "audio.levels",
            "audio.node_online",
            "audio.node_offline",
            "audio.volume_changed",
            "audio.mute_changed",
        ],
        "alert": [
            "alert.fired",
            "alert.updated",
            "alert.expired",
        ],
    }

    matching_types = filter_map.get(event_type_filter, [])
    return event_type in matching_types


async def _dispatch_event(event: dict) -> None:
    """Dispatch an event to all matching subscriber queues."""
    event_type = event.get("type", "")
    if not event_type:
        return

    # Get all registered subscriptions
    queues_map = await _subscription_registry.get_all_queues()
    filters_map = await _subscription_registry.get_all_event_types()

    for subscription_id, queue in queues_map.items():
        event_type_filter = filters_map.get(subscription_id)
        if not event_type_filter:
            continue

        # Check if event matches this subscription's filter
        if not _matches_event_type(event, event_type_filter):
            continue

        try:
            # Put event in queue (non-blocking, discard if full)
            queue.put_nowait(event)
        except asyncio.QueueFull:
            log.debug("Queue full for subscription %s, dropping event", subscription_id)
        except Exception:
            # Queue may have been closed, skip
            pass


async def _start_dispatcher(state: AppState) -> None:
    """
    Start the background task that dispatches events to subscribers.

    This runs alongside the main event pump, but specifically routes
    events to GraphQL subscription queues.
    """
    _dispatcher_task = asyncio.create_task(_dispatcher_loop(state), name="graphql-dispatcher")


async def _dispatcher_loop(state: AppState) -> None:
    """Main dispatcher loop."""
    while True:
        try:
            event = await state.events.get()
            await _dispatch_event(event)
        except asyncio.CancelledError:
            break
        except Exception as e:
            log.error("Error in GraphQL dispatcher: %s", e, exc_info=True)


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

        # Create a unique subscription ID
        subscription_id = f"node_state_{id(asyncio.current_task())}"

        # Create a queue for this subscription
        event_queue: asyncio.Queue = asyncio.Queue()

        # Register the subscription
        await _subscription_registry.register(subscription_id, event_queue, "node")

        try:
            # Send initial state snapshot
            nodes = app_state.nodes.copy()
            active = app_state.active_node_id
            yield {
                "type": "snapshot",
                "active_node_id": active,
                "nodes": [
                    NodeType.from_node(node, active=(active == node.id)).__dict__
                    for node in nodes.values()
                ],
            }

            # Drain any pre-existing events in the queue
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
            await _subscription_registry.unregister(subscription_id)

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

        # Create a unique subscription ID
        subscription_id = f"scenario_{id(asyncio.current_task())}"

        # Create a queue for this subscription
        event_queue: asyncio.Queue = asyncio.Queue()

        # Register the subscription
        await _subscription_registry.register(subscription_id, event_queue, "scenario")

        try:
            # Send initial state if scenario manager available
            if scenario_mgr:
                scenarios = scenario_mgr.list_scenarios()
                yield {
                    "type": "snapshot",
                    "scenarios": [
                        ScenarioType.from_scenario(s).__dict__ for s in scenarios
                    ],
                }

            # Yield new events
            while True:
                event = await event_queue.get()
                yield _format_scenario_event(event, scenario_mgr)

        finally:
            await _subscription_registry.unregister(subscription_id)

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

        # Create a unique subscription ID
        subscription_id = f"audio_{id(asyncio.current_task())}"

        # Create a queue for this subscription
        event_queue: asyncio.Queue = asyncio.Queue()

        # Register the subscription
        await _subscription_registry.register(subscription_id, event_queue, "audio")

        try:
            last_update_time = 0.0
            last_levels: dict[str, dict[str, float]] = {}  # node_id -> levels dict

            while True:
                event = await event_queue.get()
                current_time = time.monotonic()

                # Rate limit: only yield if AUDIO_LEVEL_INTERVAL has passed
                # and there are actual level changes
                if current_time - last_update_time >= AUDIO_LEVEL_INTERVAL:
                    formatted = _format_audio_event(event)

                    # Only yield if there are actual levels to report
                    if formatted.get("levels"):
                        yield formatted
                        last_update_time = current_time

        finally:
            await _subscription_registry.unregister(subscription_id)

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

        # Create a unique subscription ID
        subscription_id = f"alert_{id(asyncio.current_task())}"

        # Create a queue for this subscription
        event_queue: asyncio.Queue = asyncio.Queue()

        # Register the subscription
        await _subscription_registry.register(subscription_id, event_queue, "alert")

        try:
            # Send initial snapshot of pending alerts if available
            pending_alerts = getattr(app_state, "pending_alerts", None)
            if pending_alerts:
                yield {
                    "type": "snapshot",
                    "alerts": [AlertType.from_alert(a).__dict__ for a in pending_alerts],
                }

            # Yield new events
            while True:
                event = await event_queue.get()
                yield _format_alert_event(event)

        finally:
            await _subscription_registry.unregister(subscription_id)
