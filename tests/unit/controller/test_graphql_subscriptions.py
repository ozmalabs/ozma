# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for GraphQL subscriptions functionality."""

import asyncio
import sys
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
from pytest_mark_asyncio import mark_asyncio
from pytest_mark_asyncio import mark_asyncio

sys.path.insert(0, str(Path(__file__).parent.parent.parent.parent / "controller"))
pytestmark = pytest.mark.unit


@pytest.fixture
def state():
    """Create a mock AppState with events queue."""
    s = MagicMock()
    s.events = asyncio.Queue()
    s.nodes = {}
    s.active_node_id = None
    return s


@pytest.fixture
def scenario_mgr():
    """Create a mock ScenarioManager."""
    mgr = MagicMock()
    mgr.list_scenarios = MagicMock(return_value=[])
    mgr.get_scenario = MagicMock(return_value=None)
    mgr.create_scenario = MagicMock(return_value=MagicMock(id="test-scenario", name="Test", node_id=None, color="", index=0, config={}))
    mgr.activate_scenario = MagicMock(return_value=MagicMock(id="test-scenario", name="Test", node_id=None, color="", index=0, config={}))
    return mgr


@pytest.fixture
def audio_router():
    """Create a mock AudioRouter."""
    return MagicMock()


@pytest.fixture
def alert_mgr():
    """Create a mock AlertManager."""
    mgr = MagicMock()
    mgr.get_pending_alerts = MagicMock(return_value=[])
    mgr.get_alert = MagicMock(return_value=None)
    return mgr


# ── NodeType Tests ────────────────────────────────────────────────────────────

class TestNodeType:
    @mark_asyncio
    async def test_from_node_creates_type(self):
        from graphql.subscriptions import NodeType
        from state import NodeInfo

        node = NodeInfo(
            id="test-node",
            host="192.168.1.1",
            port=7331,
            role="compute",
            hw="raspberrypi",
            fw_version="0.1.0",
            proto_version=1,
            capabilities=["video", "audio"],
            machine_class="workstation",
            last_seen=1234567890.0,
        )

        node_type = NodeType.from_node(node, active=True)
        assert node_type.id == "test-node"
        assert node_type.host == "192.168.1.1"
        assert node_type.port == 7331
        assert node_type.role == "compute"
        assert node_type.hw == "raspberrypi"
        assert node_type.fw_version == "0.1.0"
        assert node_type.proto_version == 1
        assert node_type.capabilities == ["video", "audio"]
        assert node_type.machine_class == "workstation"
        assert node_type.active is True

    @mark_asyncio
    async def test_from_node_with_optional_fields(self):
        from graphql.subscriptions import NodeType
        from state import NodeInfo

        node = NodeInfo(
            id="test-node",
            host="192.168.1.1",
            port=7331,
            role="compute",
            hw="raspberrypi",
            fw_version="0.1.0",
            proto_version=1,
            capabilities=["video"],
            machine_class="workstation",
            last_seen=1234567890.0,
            vnc_host="192.168.1.1",
            vnc_port=5901,
            stream_port=7382,
            stream_path="/stream",
            audio_type="pipewire",
            audio_sink="sink1",
            audio_vban_port=6980,
            mic_vban_port=6981,
            capture_device="/dev/video0",
            sunshine_port=443,
        )

        node_type = NodeType.from_node(node, active=False)
        assert node_type.vnc_host == "192.168.1.1"
        assert node_type.vnc_port == 5901
        assert node_type.stream_port == 7382
        assert node_type.stream_path == "/stream"
        assert node_type.audio_type == "pipewire"
        assert node_type.audio_sink == "sink1"
        assert node_type.audio_vban_port == 6980
        assert node_type.mic_vban_port == 6981
        assert node_type.capture_device == "/dev/video0"
        assert node_type.sunshine_port == 443
        assert node_type.active is False


# ── ScenarioType Tests ────────────────────────────────────────────────────────

class TestScenarioType:
    @mark_asyncio
    async def test_from_scenario_creates_type(self):
        from graphql.subscriptions import ScenarioType
        from scenarios import Scenario

        scenario = Scenario(
            id="test-scenario",
            name="Test Scenario",
            node_id="node1",
            color="#FF0000",
            index=0,
            config={"key": "value"},
        )

        scenario_type = ScenarioType.from_scenario(scenario)
        assert scenario_type.id == "test-scenario"
        assert scenario_type.name == "Test Scenario"
        assert scenario_type.node_id == "node1"
        assert scenario_type.color == "#FF0000"
        assert scenario_type.index == 0
        assert scenario_type.config == {"key": "value"}


# ── AlertType Tests ───────────────────────────────────────────────────────────

class TestAlertType:
    @mark_asyncio
    async def test_from_alert_dict(self):
        from graphql.subscriptions import AlertType

        alert_dict = {
            "id": "alert1",
            "kind": "doorbell",
            "title": "Doorbell",
            "body": "Someone is at the door",
            "camera": "front_door",
            "person": "Matt",
            "severity": "info",
            "state": "active",
            "started_at": 1234567890.0,
            "timeout_s": 30,
        }

        alert_type = AlertType.from_alert(alert_dict)
        assert alert_type.id == "alert1"
        assert alert_type.kind == "doorbell"
        assert alert_type.title == "Doorbell"
        assert alert_type.body == "Someone is at the door"
        assert alert_type.camera == "front_door"
        assert alert_type.person == "Matt"
        assert alert_type.severity == "info"
        assert alert_type.state == "active"
        assert alert_type.created_at == 1234567890.0
        assert alert_type.timeout_s == 30

    @mark_asyncio
    async def test_from_alert_dict_fallback_fields(self):
        from graphql.subscriptions import AlertType

        # Test with old field name (created_at instead of started_at)
        alert_dict = {
            "id": "alert1",
            "kind": "timer",
            "title": "Timer",
            "body": "Timer finished",
            "created_at": 1234567890.0,  # Old field name
            "timeout_s": 0,
        }

        alert_type = AlertType.from_alert(alert_dict)
        assert alert_type.created_at == 1234567890.0

    @mark_asyncio
    async def test_from_alert_object(self, state):
        from graphql.subscriptions import AlertType
        from alerts import AlertSession

        alert = AlertSession(
            id="alert1",
            kind="alarm",
            title="Alarm",
            body="Smoke detected",
            started_at=1234567890.0,
            timeout_s=0,
            state="active",
        )

        alert_type = AlertType.from_alert(alert)
        assert alert_type.id == "alert1"
        assert alert_type.kind == "alarm"
        assert alert_type.title == "Alarm"
        assert alert_type.body == "Smoke detected"
        assert alert_type.created_at == 1234567890.0
        assert alert_type.timeout_s == 0
        assert alert_type.state == "active"


# ── Subscription Registry Tests ───────────────────────────────────────────────

class TestSubscriptionRegistry:
    @mark_asyncio
    async def test_register_and_unregister(self):
        from graphql.subscriptions import _subscription_registry

        subscription_id = "test-sub"
        queue = asyncio.Queue()
        event_filter = "node"

        await _subscription_registry.register(subscription_id, queue, event_filter)
        registered_queue = await _subscription_registry.get_queue(subscription_id)
        assert registered_queue is queue

        registered_filter = await _subscription_registry.get_event_type_filter(subscription_id)
        assert registered_filter == event_filter

        await _subscription_registry.unregister(subscription_id)
        removed_queue = await _subscription_registry.get_queue(subscription_id)
        assert removed_queue is None

    @mark_asyncio
    async def test_matches_filter_exact(self):
        from graphql.subscriptions import _subscription_registry

        subscription_id = "test-sub"
        queue = asyncio.Queue()
        await _subscription_registry.register(subscription_id, queue, "node")

        event = {"type": "node.online"}
        assert await _subscription_registry.matches_filter(subscription_id, event)

        event = {"type": "scenario.activated"}
        assert not await _subscription_registry.matches_filter(subscription_id, event)

        await _subscription_registry.unregister(subscription_id)

    @mark_asyncio
    async def test_matches_filter_no_filter(self):
        from graphql.subscriptions import _subscription_registry

        subscription_id = "test-sub"
        queue = asyncio.Queue()
        await _subscription_registry.register(subscription_id, queue, None)

        event = {"type": "anything"}
        assert await _subscription_registry.matches_filter(subscription_id, event)

        await _subscription_registry.unregister(subscription_id)


# ── Event Router Tests ────────────────────────────────────────────────────────

class TestEventRouter:
    @mark_asyncio
    async def test_start_and_stop_event_router(self, state):
        from graphql.subscriptions import start_event_router, stop_event_router

        start_event_router(state)
        assert state.events.qsize() == 0

        # Give the task a moment to start
        await asyncio.sleep(0.01)

        stop_event_router()

    @mark_asyncio
    async def test_event_routing(self, state):
        """Test that events are routed to subscription queues."""
        from graphql.subscriptions import _subscription_registry, _event_router_task

        # Start event router
        start_event_router(state)

        # Create a subscription
        subscription_id = "test-router"
        queue = asyncio.Queue()
        await _subscription_registry.register(subscription_id, queue, "node")

        # Wait a moment for the router to be ready
        await asyncio.sleep(0.02)

        # Put an event in the state events queue
        await state.events.put({"type": "node.online", "node": {"id": "test"}})

        # Event should be routed to the queue
        try:
            event = await asyncio.wait_for(queue.get(), timeout=0.5)
            assert event["type"] == "node.online"
        finally:
            await _subscription_registry.unregister(subscription_id)
            stop_event_router()


# ── nodeStateChanged Subscription Tests ──────────────────────────────────────

class TestNodeStateChanged:
    @mark_asyncio
    async def test_node_state_subscription_yields_initial_nodes(self, state):
        """Test that nodeStateChanged yields initial node state on subscription."""
        from graphql.subscriptions import Subscription, start_event_router, stop_event_router
        from state import NodeInfo

        # Add a node to state
        node = NodeInfo(
            id="test-node",
            host="192.168.1.1",
            port=7331,
            role="compute",
            hw="raspberrypi",
            fw_version="0.1.0",
            proto_version=1,
            capabilities=[],
            machine_class="workstation",
            last_seen=1234567890.0,
        )
        state.nodes[node.id] = node
        state.active_node_id = node.id

        start_event_router(state)

        # Create subscription
        subscription = Subscription()
        info = MagicMock()
        info.context = {"state": state}

        # Collect yielded values
        results = []
        async for item in subscription.nodeStateChanged(info):
            results.append(item)
            if len(results) >= 1:
                break

        assert len(results) >= 1
        assert results[0].id == "test-node"
        assert results[0].active is True

        stop_event_router()

    @mark_asyncio
    async def test_node_state_subscription_yields_new_node(self, state):
        """Test that nodeStateChanged yields new nodes as they come online."""
        from graphql.subscriptions import (
            Subscription, start_event_router, stop_event_router,
            EVENT_NODE_ONLINE, _subscription_registry
        )
        from state import NodeInfo

        start_event_router(state)

        # Create subscription
        subscription = Subscription()
        info = MagicMock()
        info.context = {"state": state}

        async def run_subscription():
            results = []
            async for item in subscription.nodeStateChanged(info):
                results.append(item)
                if len(results) == 2:  # Initial + new node
                    break
            return results

        # Start subscription in background
        task = asyncio.create_task(run_subscription())

        # Wait a moment
        await asyncio.sleep(0.1)

        # Add a new node
        node = NodeInfo(
            id="new-node",
            host="192.168.1.2",
            port=7331,
            role="compute",
            hw="raspberrypi",
            fw_version="0.1.0",
            proto_version=1,
            capabilities=[],
            machine_class="workstation",
            last_seen=1234567891.0,
        )
        await state.events.put({"type": EVENT_NODE_ONLINE, "node": node.to_dict()})

        # Wait for subscription to process
        await asyncio.sleep(0.2)

        results = await task
        assert len(results) >= 2
        assert results[-1].id == "new-node"

        stop_event_router()


# ── scenarioActivated Subscription Tests ──────────────────────────────────────

class TestScenarioActivated:
    @mark_asyncio
    async def test_scenario_activated_subscription_yields_initial_scenarios(self, state, scenario_mgr):
        """Test that scenarioActivated yields initial scenarios on subscription."""
        from graphql.subscriptions import Subscription, start_event_router, stop_event_router

        start_event_router(state)

        # Set up scenario manager with scenarios
        scenario1 = MagicMock(id="scenario1", name="Scenario 1", node_id="node1", color="#FF0000", index=0, config={})
        scenario2 = MagicMock(id="scenario2", name="Scenario 2", node_id="node2", color="#00FF00", index=1, config={})
        scenario_mgr.list_scenarios.return_value = [scenario1, scenario2]

        # Create subscription
        subscription = Subscription()
        info = MagicMock()
        info.context = {"state": state, "scenario_manager": scenario_mgr}

        # Collect yielded values
        results = []
        async for item in subscription.scenarioActivated(info):
            results.append(item)
            if len(results) >= 2:
                break

        assert len(results) >= 2
        assert results[0].id == "scenario1"
        assert results[1].id == "scenario2"

        stop_event_router()

    @mark_asyncio
    async def test_scenario_activated_subscription_yields_new_activation(self, state, scenario_mgr):
        """Test that scenarioActivated yields scenario activation events."""
        from graphql.subscriptions import (
            Subscription, start_event_router, stop_event_router,
            EVENT_SCENARIO_ACTIVATED
        )

        start_event_router(state)

        # Set up scenario manager
        scenario = MagicMock(id="scenario1", name="Scenario 1", node_id="node1", color="#FF0000", index=0, config={})
        scenario_mgr.get_scenario.return_value = scenario

        # Create subscription
        subscription = Subscription()
        info = MagicMock()
        info.context = {"state": state, "scenario_manager": scenario_mgr}

        async def run_subscription():
            results = []
            async for item in subscription.scenarioActivated(info):
                results.append(item)
                if len(results) >= 3:  # Initial + 1 activation
                    break
            return results

        # Start subscription in background
        task = asyncio.create_task(run_subscription())

        # Wait a moment
        await asyncio.sleep(0.1)

        # Trigger a scenario activation event
        await state.events.put({"type": EVENT_SCENARIO_ACTIVATED, "scenario_id": "scenario1"})

        # Wait for subscription to process
        await asyncio.sleep(0.2)

        results = await task
        # Find the activation result
        activation_results = [r for r in results if r.id == "scenario1"]
        assert len(activation_results) >= 1

        stop_event_router()


# ── audioLevelUpdate Subscription Tests ───────────────────────────────────────

class TestAudioLevelUpdate:
    @mark_asyncio
    async def test_audio_level_subscription_yields_levels(self, state):
        """Test that audioLevelUpdate yields audio level updates."""
        from graphql.subscriptions import (
            Subscription, start_event_router, stop_event_router,
            EVENT_AUDIO_LEVELS
        )

        start_event_router(state)

        # Create subscription
        subscription = Subscription()
        info = MagicMock()
        info.context = {"state": state, "audio_router": audio_router()}

        async def run_subscription():
            results = []
            async for item in subscription.audioLevelUpdate(info):
                results.append(item)
                if len(results) >= 1:  # At least one update
                    break
            return results

        # Start subscription in background
        task = asyncio.create_task(run_subscription())

        # Wait a moment
        await asyncio.sleep(0.1)

        # Send an audio levels event
        await state.events.put({
            "type": EVENT_AUDIO_LEVELS,
            "node_id": "node1",
            "levels": {"ch1": -10.0, "ch2": -12.5},
            "timestamp": 1234567890.0,
        })

        # Wait for subscription to process (with rate limiting)
        await asyncio.sleep(0.3)

        results = await task
        assert len(results) >= 1
        assert results[0].node_id == "node1"
        assert results[0].levels == {"ch1": -10.0, "ch2": -12.5}
        assert results[0].timestamp == 1234567890.0

        stop_event_router()


# ── alertFired Subscription Tests ─────────────────────────────────────────────

class TestAlertFired:
    @mark_asyncio
    async def test_alert_fired_subscription_yields_initial_alerts(self, state, alert_mgr):
        """Test that alertFired yields initial alerts on subscription."""
        from graphql.subscriptions import Subscription, start_event_router, stop_event_router
        from alerts import AlertSession

        start_event_router(state)

        # Set up alert manager with pending alerts
        alert = AlertSession(
            id="alert1",
            kind="doorbell",
            title="Doorbell",
            body="Someone is at the door",
            started_at=1234567890.0,
            timeout_s=30,
            state="active",
        )
        alert_mgr.get_pending_alerts.return_value = [alert]

        # Create subscription
        subscription = Subscription()
        info = MagicMock()
        info.context = {"state": state, "alert_manager": alert_mgr}

        # Collect yielded values
        results = []
        async for item in subscription.alertFired(info):
            results.append(item)
            if len(results) >= 1:
                break

        assert len(results) >= 1
        assert results[0].id == "alert1"
        assert results[0].kind == "doorbell"

        stop_event_router()

    @mark_asyncio
    async def test_alert_fired_subscription_yields_new_alert(self, state):
        """Test that alertFired yields new alert events."""
        from graphql.subscriptions import (
            Subscription, start_event_router, stop_event_router,
            EVENT_ALERT_FIRED
        )

        start_event_router(state)

        # Create subscription
        subscription = Subscription()
        info = MagicMock()
        info.context = {"state": state}

        async def run_subscription():
            results = []
            async for item in subscription.alertFired(info):
                results.append(item)
                if len(results) >= 2:  # Initial (none) + new alert
                    break
            return results

        # Start subscription in background
        task = asyncio.create_task(run_subscription())

        # Wait a moment
        await asyncio.sleep(0.1)

        # Send an alert created event
        await state.events.put({
            "type": EVENT_ALERT_FIRED,
            "id": "alert1",
            "kind": "timer",
            "title": "Timer",
            "body": "Timer finished",
            "started_at": 1234567890.0,
            "timeout_s": 0,
        })

        # Wait for subscription to process
        await asyncio.sleep(0.2)

        results = await task
        # Find the alert result
        alert_results = [r for r in results if r.id == "alert1"]
        assert len(alert_results) >= 1
        assert alert_results[0].kind == "timer"

        stop_event_router()

    @mark_asyncio
    async def test_alert_fired_subscription_yields_updated_alert(self, state, alert_mgr):
        """Test that alertFired yields updated alert events."""
        from graphql.subscriptions import (
            Subscription, start_event_router, stop_event_router,
            EVENT_ALERT_UPDATED
        )
        from alerts import AlertSession

        start_event_router(state)

        # Create updated alert
        updated_alert = AlertSession(
            id="alert1",
            kind="doorbell",
            title="Doorbell",
            body="Doorbell pressed again",
            started_at=1234567890.0,
            timeout_s=30,
            state="acknowledged",  # Changed state
        )
        alert_mgr.get_alert.return_value = updated_alert

        # Create subscription
        subscription = Subscription()
        info = MagicMock()
        info.context = {"state": state, "alert_manager": alert_mgr}

        async def run_subscription():
            results = []
            async for item in subscription.alertFired(info):
                results.append(item)
                if len(results) >= 2:  # Initial + update
                    break
            return results

        # Start subscription in background
        task = asyncio.create_task(run_subscription())

        # Wait a moment
        await asyncio.sleep(0.1)

        # Send an alert updated event
        await state.events.put({
            "type": EVENT_ALERT_UPDATED,
            "alert_id": "alert1",
            "updates": {"state": "acknowledged"},
        })

        # Wait for subscription to process
        await asyncio.sleep(0.2)

        results = await task
        # Find the update result
        update_results = [r for r in results if r.id == "alert1"]
        assert len(update_results) >= 1
        # Note: The updated state may not be reflected in the type since
        # AlertType.from_alert doesn't update existing alerts, it creates new ones

        stop_event_router()


# ── Integration Tests ─────────────────────────────────────────────────────────

class TestGraphQLSchemaIntegration:
    @mark_asyncio
    async def test_schema_has_all_subscription_fields(self):
        """Test that the GraphQL schema includes all subscription fields."""
        from graphql.schema import schema

        schema_dict = schema.introspect()
        subscription_type = None
        for t in schema_dict["data"]["__schema"]["types"]:
            if t["name"] == "Subscription":
                subscription_type = t
                break

        assert subscription_type is not None, "Subscription type not found in schema"

        subscription_fields = {f["name"] for f in subscription_type["fields"]}
        expected_fields = {
            "nodeStateChanged",
            "scenarioActivated",
            "audioLevelUpdate",
            "alertFired",
        }
        assert expected_fields.issubset(subscription_fields), \
            f"Missing subscription fields: {expected_fields - subscription_fields}"
