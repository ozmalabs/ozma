# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
GraphQL schema for Ozma Controller.

This module creates the Strawberry GraphQL schema that combines
queries, mutations, and subscriptions into a single executable schema.
"""

import logging
from typing import TYPE_CHECKING

import strawberry
from strawberry.types import Info

if TYPE_CHECKING:
    from state import AppState
    from scenarios import ScenarioManager

log = logging.getLogger("ozma.graphql.schema")


# Query type for read operations
@strawberry.type
class Query:
    """GraphQL query root type for read operations."""

    @strawberry.field
    def nodes(self, info: Info) -> list["NodeType"]:
        """List all known nodes."""
        from state import AppState
        app_state: AppState = info.context["state"]
        from .subscriptions import NodeType
        return [NodeType.from_node(node) for node in app_state.nodes.values()]

    @strawberry.field
    def node(self, info: Info, id: str) -> "NodeType | None":
        """Get a single node by ID."""
        from state import AppState
        app_state: AppState = info.context["state"]
        node = app_state.nodes.get(id)
        if node:
            from .subscriptions import NodeType
            return NodeType.from_node(node)
        return None

    @strawberry.field
    def active_node(self, info: Info) -> "NodeType | None":
        """Get the currently active node."""
        from state import AppState
        app_state: AppState = info.context["state"]
        node_id = app_state.active_node_id
        if node_id and node_id in app_state.nodes:
            from .subscriptions import NodeType
            return NodeType.from_node(app_state.nodes[node_id])
        return None

    @strawberry.field
    def snapshot(self, info: Info) -> "SnapshotType":
        """Get a full system snapshot."""
        from state import AppState
        app_state: AppState = info.context["state"]
        from .subscriptions import SnapshotType, NodeType
        nodes = [NodeType.from_node(node) for node in app_state.nodes.values()]
        return SnapshotType(
            nodes=nodes,
            active_node_id=app_state.active_node_id,
        )


# Mutation type for write operations
@strawberry.type
class Mutation:
    """GraphQL mutation root type for write operations."""

    @strawberry.field
    def activate_node(self, info: Info, node_id: str) -> "SnapshotType":
        """Activate a node and return the updated snapshot."""
        from state import AppState
        app_state: AppState = info.context["state"]
        import asyncio
        asyncio.create_task(app_state.set_active_node(node_id))
        # Return updated snapshot
        from .subscriptions import SnapshotType, NodeType
        nodes = [NodeType.from_node(node) for node in app_state.nodes.values()]
        return SnapshotType(
            nodes=nodes,
            active_node_id=app_state.active_node_id,
        )

    @strawberry.field
    def create_scenario(self, info: Info, name: str, node_id: str | None = None) -> "ScenarioType":
        """Create a new scenario."""
        scenario_mgr = info.context.get("scenario_manager")
        if scenario_mgr:
            scenario = scenario_mgr.create_scenario(name, node_id=node_id)
            from .subscriptions import ScenarioType
            return ScenarioType.from_scenario(scenario)
        raise Exception("Scenario manager not available")

    @strawberry.field
    def activate_scenario(self, info: Info, scenario_id: str) -> "ScenarioType":
        """Activate a scenario."""
        scenario_mgr = info.context.get("scenario_manager")
        if scenario_mgr:
            scenario = scenario_mgr.activate_scenario(scenario_id)
            from .subscriptions import ScenarioType
            return ScenarioType.from_scenario(scenario)
        raise Exception("Scenario manager not available")


# Import types after class definitions - use types from subscriptions.py
# to ensure proper integration with Subscription async generators
from .subscriptions import (
    NodeType, ScenarioType, AlertType, AudioLevelType, SnapshotType
)

# Create the schema
# Note: subscriptions are passed as a class, not an instance
from .subscriptions import Subscription

schema = strawberry.Schema(
    query=Query,
    mutation=Mutation,
    subscription=Subscription,
)
