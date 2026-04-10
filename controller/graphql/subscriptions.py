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
from typing import AsyncGenerator, TYPE_CHECKING, Optional, Any

import jwt  # For JWT validation

import strawberry
from strawberry.types import Info

if TYPE_CHECKING:
    from state import AppState, NodeInfo
    from scenarios import Scenario, ScenarioManager
    from audio import AudioRouter
    from alerts import AlertManager, Alert

log = logging.getLogger("ozma.graphql.subscriptions")

# Rate limit for audio level updates (10Hz = 100ms interval)
AUDIO_LEVEL_INTERVAL = 0.1  # seconds

# Event type constants
EVENT_NODE_ONLINE = "node.online"
EVENT_NODE_OFFLINE = "node.offline"
EVENT_NODE_SWITCHED = "node.switched"
EVENT_SCENARIO_ACTIVATED = "scenario.activated"
EVENT_AUDIO_LEVELS = "audio.levels"
EVENT_ALERT_FIRED = "alert.created"
EVENT_ALERT_UPDATED = "alert.updated"


@strawberry.type
class NodeType:
    """
    GraphQL type for a node in the KVMA system.

    Represents a hardware node (SBC or VM) that is permanently wired to
    one target machine via USB (HID gadget) and optionally HDMI.
    """

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
    display_outputs: list["JSONScalar"] = strawberry.field(default_factory=list)
    vnc_host: str | None = None
    vnc_port: int | None = None
    stream_port: int | None = None
    stream_path: str | None = None
    api_port: int | None = None
    audio_type: str | None = None
    audio_sink: str | None = None
    audio_vban_port: int | None = None
    mic_vban_port: int | None = None
    capture_device: str | None = None
    camera_streams: list["JSONScalar"] = strawberry.field(default_factory=list)
    frigate_host: str | None = None
    frigate_port: int | None = None
    owner_user_id: str = ""
    owner_id: str = ""
    shared_with: list[str] = strawberry.field(default_factory=list)
    seat_count: int = 1
    seat_config: "JSONScalar" = strawberry.field(default_factory=dict)
    parent_node_id: str = ""
    sunshine_port: int | None = None
    active: bool = False

    @staticmethod
    def from_node(node: "NodeInfo", active: bool = False) -> "NodeType":
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
    """
    GraphQL type for a scenario.

    A scenario represents a saved configuration that can be activated,
    typically containing node assignment, color, and custom configuration.
    """

    id: str
    name: str
    node_id: str | None = None
    color: str = ""
    index: int = 0
    config: "JSONScalar" = strawberry.field(default_factory=dict)

    @staticmethod
    def from_scenario(scenario: "Scenario") -> "ScenarioType":
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
    """
    GraphQL type for an alert.

    Alerts represent system events that require attention, such as
    camera motion detection, device status changes, or error conditions.
    """

    id: str
    kind: str = ""
    title: str = ""
    body: str = ""
    camera: str | None = None
    person: str | None = None
    severity: str = ""
    state: str = ""
    created_at: float = 0.0
    timeout_s: float = 0.0
    camera_id: str | None = None

    @staticmethod
    def from_alert(alert: "Alert | dict[str, Any]") -> "AlertType":
        """Convert internal Alert or alert dict to GraphQL AlertType."""
        # Handle both AlertSession objects and dict payloads from events
        if isinstance(alert, dict):
            return AlertType(
                id=alert.get("id", ""),
                kind=alert.get("kind", ""),
                title=alert.get("title", ""),
                body=alert.get("body", ""),
                camera=alert.get("camera"),
                person=alert.get("person"),
                severity=alert.get("severity", ""),
                state=alert.get("state", ""),
                created_at=alert.get("started_at", alert.get("created_at", 0.0)),
                timeout_s=alert.get("timeout_s", 0.0),
                camera_id=alert.get("camera"),
            )
        return AlertType(
            id=alert.id,
            kind=alert.kind,
            title=alert.title,
            body=alert.body,
            camera=alert.camera,
            person=alert.person,
            severity="",
            state=alert.state,
            created_at=alert.started_at,
            timeout_s=alert.timeout_s,
            camera_id=alert.camera,
        )


from typing import Annotated

@strawberry.type
class AudioLevelType:
    """
    GraphQL type for audio level data for a single node.

    Contains per-channel dB measurements for audio monitoring.
    """

    node_id: str = ""
    levels: "JSONScalar" = strawberry.field(
        description="Mapping of channel names to dB values"
    )
    timestamp: float = 0.0


@strawberry.type
class SnapshotType:
    """GraphQL type for system snapshot."""

    nodes: list[NodeType]
    active_node_id: str | None


# Custom scalar for JSON types (dict/list)
@strawberry.scalar
class JSONScalar:
    """Custom scalar for JSON-serializable data."""

    @staticmethod
    def serialize(value: Any) -> Any:
        return value

    @staticmethod
    def parse_literal(value: Any) -> Any:
        return value

    @staticmethod
    def parse_value(value: Any) -> Any:
        return value


# Global subscription registry - manages event routing to subscriptions
class _SubscriptionRegistry:
    """
    Thread-safe registry for subscription queues and event routing.

    This registry:
    1. Tracks all active subscriptions with their IDs and event type filters
    2. Routes events from AppState.events to matching subscription queues
    3. Provides cleanup when subscriptions are terminated
    """

    def __init__(self):
        self._queues: dict[str, asyncio.Queue] = {}
        self._event_type_filters: dict[str, str | None] = {}  # subscription_id -> event_type_filter
        self._lock = asyncio.Lock()

    async def register(
        self, subscription_id: str, queue: asyncio.Queue, event_type_filter: str | None = None
    ) -> None:
        """
        Register a new subscription.

        Args:
            subscription_id: Unique identifier for this subscription
            queue: Asyncio queue to deliver events to
            event_type_filter: Optional event type filter (e.g., "node", "scenario", "audio")
        """
        async with self._lock:
            self._queues[subscription_id] = queue
            self._event_type_filters[subscription_id] = event_type_filter
            log.debug("Registered subscription %s with filter '%s'", subscription_id, event_type_filter)

    async def unregister(self, subscription_id: str) -> None:
        """
        Unregister a subscription and clean up resources.

        Args:
            subscription_id: The subscription ID to remove
        """
        async with self._lock:
            self._queues.pop(subscription_id, None)
            self._event_type_filters.pop(subscription_id, None)
            log.debug("Unregistered subscription %s", subscription_id)

    async def get_queue(self, subscription_id: str) -> asyncio.Queue | None:
        """Get a subscription queue by ID."""
        async with self._lock:
            return self._queues.get(subscription_id)

    async def get_event_type_filter(self, subscription_id: str) -> str | None:
        """Get the event type filter for a subscription."""
        async with self._lock:
            return self._event_type_filters.get(subscription_id)

    async def get_all_queues(self) -> dict[str, asyncio.Queue]:
        """Get all subscription queues."""
        async with self._lock:
            return dict(self._queues)

    async def matches_filter(self, subscription_id: str, event: dict[str, Any]) -> bool:
        """
        Check if an event matches a subscription's filter.

        Args:
            subscription_id: The subscription ID
            event: The event to check

        Returns:
            True if the event matches the filter or if no filter is set
        """
        async with self._lock:
            event_filter = self._event_type_filters.get(subscription_id)
            if event_filter is None:
                return True
            event_type = event.get("type", "")
            return event_type.startswith(event_filter + ".") or event_type == event_filter


# Global registry instance
_subscription_registry = _SubscriptionRegistry()


# Global event router task reference
_event_router_task: asyncio.Task | None = None


async def _event_router_worker(state: "AppState") -> None:
    """
    Background task that routes events from AppState to subscription queues.

    This task:
    1. Consumes events from state.events
    2. Routes matching events to all registered subscription queues
    3. Runs until cancelled

    Args:
        state: The AppState instance containing the events queue
    """
    log.info("Starting event router task")
    try:
        while True:
            event = await state.events.get()
            event_type = event.get("type", "")

            # Route event to matching subscription queues
            async with _subscription_registry._lock:
                matching_queues = []
                for sub_id, queue in _subscription_registry._queues.items():
                    event_filter = _subscription_registry._event_type_filters.get(sub_id)
                    if event_filter is None:
                        matching_queues.append((sub_id, queue))
                    elif event_type.startswith(event_filter + ".") or event_type == event_filter:
                        matching_queues.append((sub_id, queue))

            # Route event to matching queues (non-blocking)
            for sub_id, queue in matching_queues:
                try:
                    queue.put_nowait(event)
                except asyncio.QueueFull:
                    log.debug("Queue full for subscription %s, dropping event %s", sub_id, event_type)

    except asyncio.CancelledError:
        log.info("Event router task cancelled")
        raise


def start_event_router(state: "AppState") -> None:
    """
    Start the global event router task that routes events to subscriptions.

    This should be called once during application startup.

    Args:
        state: The AppState instance containing the events queue
    """
    global _event_router_task
    if _event_router_task is None:
        _event_router_task = asyncio.create_task(
            _event_router_worker(state), name="graphql-event-router"
        )


def stop_event_router() -> None:
    """Stop the global event router task."""
    global _event_router_task
    if _event_router_task is not None:
        _event_router_task.cancel()
        _event_router_task = None


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
            nodeStateChanged { id host active }
            scenarioActivated { id name color }
            audioLevelUpdate { node_id levels timestamp }
            alertFired { id kind severity title }
        }
    """

    @strawberry.subscription
    async def nodeStateChanged(
        self,
        info: Info,
    ) -> AsyncGenerator[NodeType, None]:
        """
        Subscribe to node state changes.

        Yields NodeType when a node comes online or goes offline.

        Events consumed:
            - node.online: node added to the system
            - node.offline: node removed from the system
            - node.switched: active node changed

        The active field in each yielded object indicates whether this
        node is currently active (None if no active node).
        """
        from state import AppState, NodeInfo

        app_state: AppState = info.context["state"]
        active_node_id = app_state.active_node_id

        # Create a unique subscription ID
        subscription_id = f"node_state_{id(asyncio.current_task())}"

        # Create a queue for this subscription
        event_queue: asyncio.Queue = asyncio.Queue()

        # Register the subscription with node event filter
        await _subscription_registry.register(subscription_id, event_queue, "node")

        try:
            # Send initial state snapshot with active node indicator
            nodes = app_state.nodes.copy()
            active = app_state.active_node_id
            for node in nodes.values():
                yield NodeType.from_node(node, active=(active == node.id))

            # Yield new events as they arrive
            while True:
                event = await event_queue.get()
                event_type = event.get("type", "")

                if event_type == EVENT_NODE_ONLINE:
                    node_dict = event.get("node", {})
                    temp_node = NodeInfo(
                        id=node_dict.get("id", ""),
                        host=node_dict.get("host", ""),
                        port=node_dict.get("port", 0),
                        role=node_dict.get("role", ""),
                        hw=node_dict.get("hw", ""),
                        fw_version=node_dict.get("fw_version", ""),
                        proto_version=node_dict.get("proto_version", 1),
                        capabilities=node_dict.get("capabilities", []),
                        machine_class=node_dict.get("machine_class", "workstation"),
                        last_seen=node_dict.get("last_seen", time.monotonic()),
                        display_outputs=node_dict.get("display_outputs", []),
                        vnc_host=node_dict.get("vnc_host"),
                        vnc_port=node_dict.get("vnc_port"),
                        stream_port=node_dict.get("stream_port"),
                        stream_path=node_dict.get("stream_path"),
                        api_port=node_dict.get("api_port"),
                        audio_type=node_dict.get("audio_type"),
                        audio_sink=node_dict.get("audio_sink"),
                        audio_vban_port=node_dict.get("audio_vban_port"),
                        mic_vban_port=node_dict.get("mic_vban_port"),
                        capture_device=node_dict.get("capture_device"),
                        camera_streams=node_dict.get("camera_streams", []),
                        frigate_host=node_dict.get("frigate_host"),
                        frigate_port=node_dict.get("frigate_port"),
                        owner_user_id=node_dict.get("owner_user_id", ""),
                        owner_id=node_dict.get("owner_id", ""),
                        shared_with=node_dict.get("shared_with", []),
                        seat_count=node_dict.get("seat_count", 1),
                        seat_config=node_dict.get("seat_config", {}),
                        parent_node_id=node_dict.get("parent_node_id", ""),
                        sunshine_port=node_dict.get("sunshine_port"),
                        direct_registered=node_dict.get("direct_registered", False),
                    )
                    yield NodeType.from_node(temp_node, active=(active == temp_node.id))
                elif event_type == EVENT_NODE_OFFLINE:
                    node_id = event.get("node_id", "")
                    yield NodeType.from_node(NodeInfo(
                        id=node_id,
                        host="",
                        port=0,
                        role="",
                        hw="",
                        fw_version="",
                        proto_version=1,
                        machine_class="",
                    ), active=False)
                elif event_type == EVENT_NODE_SWITCHED:
                    node_id = event.get("node_id", "")
                    if node_id in app_state.nodes:
                        yield NodeType.from_node(app_state.nodes[node_id], active=True)

        except asyncio.CancelledError:
            log.debug("Subscription cancelled: nodeStateChanged")
            raise
        finally:
            # Cleanup subscription
            await _subscription_registry.unregister(subscription_id)

    @strawberry.subscription
    async def scenarioActivated(
        self,
        info: Info,
    ) -> AsyncGenerator[ScenarioType, None]:
        """
        Subscribe to scenario activation events.

        Yields ScenarioType when a scenario is activated.

        Events consumed:
            - scenario.activated: scenario switching occurred

        The yielded object contains the scenario details including
        id, name, node_id, and configuration.
        """
        from state import AppState
        from scenarios import ScenarioManager

        app_state: AppState = info.context["state"]
        scenario_mgr: ScenarioManager | None = info.context.get("scenario_manager")

        # Create a unique subscription ID
        subscription_id = f"scenario_{id(asyncio.current_task())}"

        # Create a queue for this subscription
        event_queue: asyncio.Queue = asyncio.Queue()

        # Register the subscription with scenario event filter
        await _subscription_registry.register(subscription_id, event_queue, "scenario")

        try:
            # Send initial state if scenario manager available
            if scenario_mgr:
                scenarios = scenario_mgr.list_scenarios()
                for scenario in scenarios:
                    yield ScenarioType.from_scenario(scenario)

            # Yield new events
            while True:
                event = await event_queue.get()
                event_type = event.get("type", "")

                if event_type == EVENT_SCENARIO_ACTIVATED:
                    scenario_id = event.get("scenario_id")
                    if scenario_id and scenario_mgr:
                        scenario = scenario_mgr.get_scenario(scenario_id)
                        if scenario:
                            yield ScenarioType.from_scenario(scenario)

        except asyncio.CancelledError:
            log.debug("Subscription cancelled: scenarioActivated")
            raise
        finally:
            await _subscription_registry.unregister(subscription_id)

    @strawberry.subscription
    async def audioLevelUpdate(
        self,
        info: Info,
    ) -> AsyncGenerator[AudioLevelType, None]:
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
        from state import AppState
        from audio import AudioRouter

        app_state: AppState = info.context["state"]
        audio_router: AudioRouter | None = info.context.get("audio_router")

        # Create a unique subscription ID
        subscription_id = f"audio_{id(asyncio.current_task())}"

        # Create a queue for this subscription
        event_queue: asyncio.Queue = asyncio.Queue()

        # Register the subscription with audio event filter
        await _subscription_registry.register(subscription_id, event_queue, "audio")

        try:
            last_update_time = 0.0

            while True:
                event = await event_queue.get()
                current_time = time.monotonic()

                # Rate limit: only yield if AUDIO_LEVEL_INTERVAL has passed
                if current_time - last_update_time >= AUDIO_LEVEL_INTERVAL:
                    # Extract node_id and levels from event
                    node_id = event.get("node_id", "")
                    levels = event.get("levels", {})
                    timestamp = event.get("timestamp", current_time)

                    # Only yield if there are actual levels to report
                    if levels:
                        yield AudioLevelType(
                            node_id=node_id,
                            levels=levels,
                            timestamp=timestamp,
                        )
                        last_update_time = current_time

        except asyncio.CancelledError:
            log.debug("Subscription cancelled: audioLevelUpdate")
            raise
        finally:
            await _subscription_registry.unregister(subscription_id)

    @strawberry.subscription
    async def alertFired(
        self,
        info: Info,
    ) -> AsyncGenerator[AlertType, None]:
        """
        Subscribe to alert events.

        Yields AlertType when an alert is created or updated in the system.

        Events consumed:
            - alert.created: new alert raised
            - alert.acknowledged: alert acknowledged
            - alert.dismissed: alert dismissed
            - alert.updated: alert state changed
            - alert.expired: alert expired

        Each yielded object contains the full alert details including
        id, kind, title, body, camera, person, severity, state, and timestamps.
        """
        from state import AppState
        from alerts import AlertManager

        app_state: AppState = info.context["state"]

        # Create a unique subscription ID
        subscription_id = f"alert_{id(asyncio.current_task())}"

        # Create a queue for this subscription
        event_queue: asyncio.Queue = asyncio.Queue()

        # Register the subscription with alert event filter
        await _subscription_registry.register(subscription_id, event_queue, "alert")

        try:
            # Send initial snapshot of pending alerts if available
            alert_mgr: AlertManager | None = info.context.get("alert_manager")
            if alert_mgr:
                pending_alerts = alert_mgr.get_pending_alerts()
                for alert in pending_alerts:
                    yield AlertType.from_alert(alert)

            # Yield new events
            while True:
                event = await event_queue.get()
                event_type = event.get("type", "")

                if event_type == EVENT_ALERT_FIRED:
                    # alert.created events include alert data directly in the event
                    yield AlertType.from_alert(event)
                elif event_type == EVENT_ALERT_UPDATED:
                    alert_id = event.get("alert_id", "")
                    updates = event.get("updates", {})

                    # If we have an alert manager, get the updated alert
                    if alert_mgr:
                        updated_alert = alert_mgr.get_alert(alert_id)
                        if updated_alert:
                            yield AlertType.from_alert(updated_alert)
                        else:
                            # Apply updates manually if alert not found
                            alert_dict = dict(event)
                            for key, value in updates.items():
                                alert_dict[key] = value
                            yield AlertType.from_alert(alert_dict)

        except asyncio.CancelledError:
            log.debug("Subscription cancelled: alertFired")
            raise
        finally:
            await _subscription_registry.unregister(subscription_id)
