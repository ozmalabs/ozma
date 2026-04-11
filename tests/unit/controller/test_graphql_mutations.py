# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Tests for GraphQL mutations.

Tests all mutation operations for nodes, scenarios, audio routing, and system state.
"""

import asyncio
import pytest
from unittest.mock import AsyncMock, MagicMock, patch

from controller.graphql import (
    Mutation,
    GraphQLError,
    Node,
    Scenario,
    AudioNode,
    AudioRoute,
    WakeOnLanResult,
    DeleteResult,
    ScenarioInput,
    TransitionInput,
    BluetoothConfigInput,
    MotionPresetInput,
    WallpaperInput,
    _require_write_scope,
    _node_to_graphql,
    _scenario_to_graphql,
    _get_audio_node,
)
from controller.state import AppState, NodeInfo
from controller.scenarios import ScenarioManager, Scenario, TransitionConfig


# ──────────────────────────────────────────────────────────────────────────────
# Fixtures
# ──────────────────────────────────────────────────────────────────────────────

@pytest.fixture
def mock_info():
    """Create a mock GraphQL info context."""
    info = MagicMock()
    info.context = MagicMock()
    info.context.state = None
    info.context.scenario_manager = None
    info.context.audio_router = None
    return info


@pytest.fixture
def app_state():
    """Create an AppState with test nodes."""
    state = AppState()
    # Add test nodes
    node1 = NodeInfo(
        id="node-1._ozma._udp.local.",
        host="192.168.1.10",
        port=7331,
        role="compute",
        hw="rpi-zero2w",
        fw_version="1.0.0",
        proto_version=1,
        capabilities=["hid", "video", "audio"],
        machine_class="workstation",
        name="Workstation A",
        audio_type="pipewire",
        audio_sink="ozma-vm1",
        audio_vban_port=6980,
    )
    node2 = NodeInfo(
        id="node-2._ozma._udp.local.",
        host="192.168.1.11",
        port=7331,
        role="compute",
        hw="rpi-zero2w",
        fw_version="1.0.0",
        proto_version=1,
        capabilities=["hid", "video"],
        machine_class="server",
        name="Workstation B",
    )
    state.nodes[node1.id] = node1
    state.nodes[node2.id] = node2
    return state


@pytest.fixture
def scenario_manager(app_state):
    """Create a ScenarioManager with test scenarios."""
    import tempfile
    from pathlib import Path

    with tempfile.NamedTemporaryFile(mode='w', suffix='.json', delete=False) as f:
        f.write('{"scenarios": [{"id": "work", "name": "Work", "node_id": "node-1._ozma._udp.local.", "color": "#4A90D9"}]}')
        config_path = Path(f.name)

    mgr = ScenarioManager(
        config_path=config_path,
        state=app_state,
    )
    # Load scenarios
    mgr._load()
    return mgr


@pytest.fixture
def auth_info(mock_info, app_state, scenario_manager):
    """Create a mock info with authentication context."""
    mock_info.context.state = app_state
    mock_info.context.scenario_manager = scenario_manager
    mock_info.context.audio_router = None

    # Create a mock auth context with write scope
    auth = MagicMock()
    auth.authenticated = True
    auth.scopes = ["read", "write"]
    mock_info.context.auth = auth

    return mock_info


# ──────────────────────────────────────────────────────────────────────────────
# Unit Tests
# ──────────────────────────────────────────────────────────────────────────────

class TestNodeMutations:
    """Tests for node-related mutations."""

    @pytest.mark.asyncio
    async def test_activate_node_success(self, auth_info, app_state):
        """Test successful node activation."""
        mutation = Mutation()

        result = await mutation.activate_node(auth_info, id="node-1._ozma._udp.local.")

        assert isinstance(result, Node)
        assert result.id == "node-1._ozma._udp.local."
        assert result.name == "Workstation A"
        assert app_state.active_node_id == "node-1._ozma._udp.local."

    @pytest.mark.asyncio
    async def test_activate_node_not_found(self, auth_info):
        """Test activation of non-existent node."""
        mutation = Mutation()

        with pytest.raises(GraphQLError) as exc_info:
            await mutation.activate_node(auth_info, id="non-existent._ozma._udp.local.")

        assert exc_info.value.code == "NODE_NOT_FOUND"
        assert "non-existent" in exc_info.value.message

    @pytest.mark.asyncio
    async def test_rename_node_success(self, auth_info):
        """Test successful node renaming."""
        mutation = Mutation()

        result = await mutation.rename_node(
            auth_info,
            id="node-1._ozma._udp.local.",
            name="New Name"
        )

        assert isinstance(result, Node)
        assert result.name == "New Name"
        assert app_state.nodes["node-1._ozma._udp.local."].name == "New Name"

    @pytest.mark.asyncio
    async def test_rename_node_empty_name(self, auth_info):
        """Test renaming with empty name."""
        mutation = Mutation()

        with pytest.raises(GraphQLError) as exc_info:
            await mutation.rename_node(
                auth_info,
                id="node-1._ozma._udp.local.",
                name=""
            )

        assert exc_info.value.code == "INVALID_NAME"

    @pytest.mark.asyncio
    async def test_rename_node_not_found(self, auth_info):
        """Test renaming non-existent node."""
        mutation = Mutation()

        with pytest.raises(GraphQLError) as exc_info:
            await mutation.rename_node(
                auth_info,
                id="non-existent._ozma._udp.local.",
                name="New Name"
            )

        assert exc_info.value.code == "NODE_NOT_FOUND"


class TestWakeOnLanMutation:
    """Tests for Wake-on-LAN mutation."""

    @pytest.mark.asyncio
    async def test_wake_on_lan_success(self, auth_info, app_state):
        """Test successful Wake-on-LAN."""
        with patch('controller.graphql.get_mac_from_arp', return_value="aa:bb:cc:dd:ee:ff"):
            with patch('controller.graphql.send_wol', return_value=True):
                mutation = Mutation()

                result = await mutation.wake_on_lan(auth_info, id="node-1._ozma._udp.local.")

                assert isinstance(result, WakeOnLanResult)
                assert result.success is True
                assert result.mac == "aa:bb:cc:dd:ee:ff"

    @pytest.mark.asyncio
    async def test_wake_on_lan_no_mac(self, auth_info):
        """Test Wake-on-LAN when MAC is not available."""
        with patch('controller.graphql.get_mac_from_arp', return_value=None):
            mutation = Mutation()

            result = await mutation.wake_on_lan(auth_info, id="node-1._ozma._udp.local.")

            assert isinstance(result, WakeOnLanResult)
            assert result.success is False
            assert result.mac is None

    @pytest.mark.asyncio
    async def test_wake_on_lan_not_found(self, auth_info):
        """Test Wake-on-LAN for non-existent node."""
        with patch('controller.graphql.get_mac_from_arp', return_value=None):
            mutation = Mutation()

            with pytest.raises(GraphQLError) as exc_info:
                await mutation.wake_on_lan(auth_info, id="non-existent._ozma._udp.local.")

            assert exc_info.value.code == "NODE_NOT_FOUND"


class TestScenarioMutations:
    """Tests for scenario-related mutations."""

    @pytest.mark.asyncio
    async def test_create_scenario_success(self, auth_info, app_state):
        """Test successful scenario creation."""
        mutation = Mutation()
        input_data = ScenarioInput(
            id="gaming",
            name="Gaming",
            node_id="node-2._ozma._udp.local.",
            color="#FF6B6B"
        )

        result = await mutation.create_scenario(auth_info, input=input_data)

        assert isinstance(result, Scenario)
        assert result.id == "gaming"
        assert result.name == "Gaming"
        assert result.node_id == "node-2._ozma._udp.local."

    @pytest.mark.asyncio
    async def test_create_scenario_duplicate(self, auth_info, scenario_manager):
        """Test creating a scenario that already exists."""
        # Need to update auth_info to use scenario_manager
        auth_info.context.scenario_manager = scenario_manager

        mutation = Mutation()
        input_data = ScenarioInput(
            id="work",  # Already exists
            name="Work Duplicate",
        )

        with pytest.raises(GraphQLError) as exc_info:
            await mutation.create_scenario(auth_info, input=input_data)

        assert exc_info.value.code == "SCENARIO_EXISTS"

    @pytest.mark.asyncio
    async def test_create_scenario_invalid_name(self, auth_info, app_state):
        """Test creating scenario with invalid name."""
        auth_info.context.state = app_state

        mutation = Mutation()
        input_data = ScenarioInput(
            id="test",
            name="",  # Empty name
        )

        with pytest.raises(GraphQLError) as exc_info:
            await mutation.create_scenario(auth_info, input=input_data)

        assert exc_info.value.code == "INVALID_INPUT"

    @pytest.mark.asyncio
    async def test_update_scenario_success(self, auth_info, scenario_manager):
        """Test successful scenario update."""
        auth_info.context.scenario_manager = scenario_manager

        mutation = Mutation()
        input_data = ScenarioInput(
            id="work",
            name="Work Updated",
            node_id="node-2._ozma._udp.local.",
            color="#4A90D9"
        )

        result = await mutation.update_scenario(auth_info, id="work", input=input_data)

        assert isinstance(result, Scenario)
        assert result.name == "Work Updated"
        assert result.node_id == "node-2._ozma._udp.local."

    @pytest.mark.asyncio
    async def test_update_scenario_not_found(self, auth_info):
        """Test updating non-existent scenario."""
        mutation = Mutation()
        input_data = ScenarioInput(
            id="nonexistent",
            name="Non-existent",
        )

        with pytest.raises(GraphQLError) as exc_info:
            await mutation.update_scenario(auth_info, id="nonexistent", input=input_data)

        assert exc_info.value.code == "SCENARIO_NOT_FOUND"

    @pytest.mark.asyncio
    async def test_delete_scenario_success(self, auth_info, scenario_manager):
        """Test successful scenario deletion."""
        auth_info.context.scenario_manager = scenario_manager

        mutation = Mutation()

        result = await mutation.delete_scenario(auth_info, id="work")

        assert isinstance(result, DeleteResult)
        assert result.success is True
        assert result.deleted_id == "work"

    @pytest.mark.asyncio
    async def test_delete_scenario_not_found(self, auth_info):
        """Test deleting non-existent scenario."""
        mutation = Mutation()

        with pytest.raises(GraphQLError) as exc_info:
            await mutation.delete_scenario(auth_info, id="nonexistent")

        assert exc_info.value.code == "SCENARIO_NOT_FOUND"

    @pytest.mark.asyncio
    async def test_activate_scenario_success(self, auth_info, scenario_manager):
        """Test successful scenario activation."""
        auth_info.context.scenario_manager = scenario_manager

        mutation = Mutation()

        result = await mutation.activate_scenario(auth_info, id="work")

        assert isinstance(result, Scenario)
        assert result.id == "work"


class TestAudioMutations:
    """Tests for audio-related mutations."""

    @pytest.mark.asyncio
    async def test_set_audio_volume_success(self, auth_info, app_state):
        """Test successful audio volume setting."""
        auth_info.context.state = app_state

        # Create mock audio router
        audio_router = AsyncMock()
        audio_router.set_volume = AsyncMock(return_value=True)
        auth_info.context.audio_router = audio_router

        mutation = Mutation()

        result = await mutation.set_audio_volume(auth_info, node_id="node-1._ozma._udp.local.", volume=0.75)

        assert isinstance(result, AudioNode)
        assert result.node_id == "node-1._ozma._udp.local."
        assert result.volume == 0.75

    @pytest.mark.asyncio
    async def test_set_audio_volume_invalid_range(self, auth_info):
        """Test setting volume out of range."""
        mutation = Mutation()

        with pytest.raises(GraphQLError) as exc_info:
            await mutation.set_audio_volume(auth_info, node_id="node-1._ozma._udp.local.", volume=1.5)

        assert exc_info.value.code == "INVALID_VOLUME"

    @pytest.mark.asyncio
    async def test_set_audio_volume_node_not_found(self, auth_info):
        """Test setting volume for non-existent node."""
        mutation = Mutation()

        with pytest.raises(GraphQLError) as exc_info:
            await mutation.set_audio_volume(auth_info, node_id="non-existent._ozma._udp.local.", volume=0.5)

        assert exc_info.value.code == "NODE_NOT_FOUND"

    @pytest.mark.asyncio
    async def test_mute_node_success(self, auth_info, app_state):
        """Test successful node mute."""
        auth_info.context.state = app_state

        # Create mock audio router
        audio_router = AsyncMock()
        audio_router.set_mute = AsyncMock(return_value=True)
        auth_info.context.audio_router = audio_router

        mutation = Mutation()

        result = await mutation.mute_node(auth_info, node_id="node-1._ozma._udp.local.", muted=True)

        assert isinstance(result, AudioNode)
        assert result.node_id == "node-1._ozma._udp.local."
        assert result.muted is True

    @pytest.mark.asyncio
    async def test_set_audio_route_success(self, auth_info, app_state):
        """Test successful audio route setting."""
        auth_info.context.state = app_state

        mutation = Mutation()

        result = await mutation.set_audio_route(
            auth_info,
            source="node-1._ozma._udp.local.",
            target="node-2._ozma._udp.local.",
            active=True
        )

        assert isinstance(result, AudioRoute)
        assert result.source_id == "node-1._ozma._udp.local."
        assert result.target_id == "node-2._ozma._udp.local."
        assert result.active is True

    @pytest.mark.asyncio
    async def test_set_audio_route_source_not_found(self, auth_info):
        """Test setting route with non-existent source."""
        mutation = Mutation()

        with pytest.raises(GraphQLError) as exc_info:
            await mutation.set_audio_route(
                auth_info,
                source="non-existent._ozma._udp.local.",
                target="node-2._ozma._udp.local.",
                active=True
            )

        assert exc_info.value.code == "NODE_NOT_FOUND"


class TestHelperFunctions:
    """Tests for helper functions."""

    def test_node_to_graphql(self, app_state):
        """Test NodeInfo to GraphQL Node conversion."""
        node = app_state.nodes["node-1._ozma._udp.local."]
        result = _node_to_graphql(node)

        assert isinstance(result, Node)
        assert result.id == node.id
        assert result.name == node.name
        assert result.host == node.host

    def test_scenario_to_graphql(self, scenario_manager):
        """Test Scenario to GraphQL Scenario conversion."""
        scenario = scenario_manager.get("work")
        result = _scenario_to_graphql(scenario)

        assert isinstance(result, Scenario)
        assert result.id == scenario.id
        assert result.name == scenario.name


class TestErrorHandling:
    """Tests for error handling in mutations."""

    @pytest.mark.asyncio
    async def test_missing_auth_context(self):
        """Test mutation without auth context."""
        info = MagicMock()
        info.context = MagicMock()
        info.context.state = None

        mutation = Mutation()

        # This should raise GraphQLError due to missing auth
        with pytest.raises(GraphQLError):
            await mutation.activate_node(info, id="node-1._ozma._udp.local.")

    @pytest.mark.asyncio
    async def test_missing_write_scope(self):
        """Test mutation without write scope."""
        info = MagicMock()
        info.context = MagicMock()

        auth = MagicMock()
        auth.authenticated = True
        auth.scopes = ["read"]  # No write scope
        info.context.auth = auth
        info.context.state = None

        mutation = Mutation()

        with pytest.raises(GraphQLError) as exc_info:
            await mutation.activate_node(info, id="node-1._ozma._udp.local.")

        assert exc_info.value.code == "UNAUTHORIZED"

    @pytest.mark.asyncio
    async def test_missing_state(self):
        """Test mutation when AppState is not available."""
        info = MagicMock()
        info.context = MagicMock()

        auth = MagicMock()
        auth.authenticated = True
        auth.scopes = ["read", "write"]
        info.context.auth = auth
        info.context.state = None

        mutation = Mutation()

        with pytest.raises(GraphQLError) as exc_info:
            await mutation.activate_node(info, id="node-1._ozma._udp.local.")

        assert exc_info.value.code == "INTERNAL_ERROR"
