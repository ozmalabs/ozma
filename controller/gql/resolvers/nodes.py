# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Resolvers for node-related GraphQL queries.
"""

from typing import Optional

import strawberry
from strawberry.types import Info

from controller.state import NodeInfo
from controller.scenarios import ScenarioManager


@strawberry.type
class QueryNodes:
    """Query resolvers for nodes."""

    @strawberry.field
    def nodes(self, info: Info) -> list["NodeType"]:
        """Query all known nodes."""
        app_state = info.context.get("app_state")
        if not app_state:
            return []

        nodes_list = []
        for node_id in sorted(app_state.nodes.keys()):
            node = app_state.nodes[node_id]
            from controller.gql.types.nodes import NodeType

            nodes_list.append(NodeType.from_nodeinfo(node))
        return nodes_list

    @strawberry.field
    def node(self, info: Info, id: str) -> Optional["NodeType"]:
        """Query a single node by ID."""
        app_state = info.context.get("app_state")
        if not app_state:
            return None

        node = app_state.nodes.get(id)
        if not node:
            return None

        from controller.gql.types.nodes import NodeType

        return NodeType.from_nodeinfo(node)

    @strawberry.field
    def active_node(self, info: Info) -> Optional["NodeType"]:
        """Query the currently active node."""
        app_state = info.context.get("app_state")
        if not app_state:
            return None

        node_id = app_state.active_node_id
        if not node_id:
            return None

        node = app_state.nodes.get(node_id)
        if not node:
            return None

        from controller.gql.types.nodes import NodeType

        return NodeType.from_nodeinfo(node)


# Import NodeType after class definition to avoid circular import
from controller.gql.types.nodes import NodeType
