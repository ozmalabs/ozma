# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""GraphQL resolvers for Node queries."""

from typing import Any

import strawberry

from ..types.nodes import NodeType
from ..state import AppState


@strawberry.type
class QueryNodes:
    """Query resolvers for node-related operations."""

    @strawberry.field
    def nodes(self, info: strawberry.Info) -> list[NodeType]:
        """Query all nodes."""
        app_state = info.context.get("app_state")
        if not app_state:
            return []

        nodes_list = []
        for node_id in app_state.nodes:
            node = app_state.nodes[node_id]
            nodes_list.append(NodeType.from_nodeinfo(node))
        return nodes_list

    @strawberry.field
    def node(self, info: strawberry.Info, id: str) -> NodeType | None:
        """Query a single node by ID."""
        app_state = info.context.get("app_state")
        if not app_state:
            return None

        # Strip the strawberry.ID wrapper if present
        node_id = str(id)

        if node_id in app_state.nodes:
            return NodeType.from_nodeinfo(app_state.nodes[node_id])
        return None

    @strawberry.field
    def active_node(self, info: strawberry.Info) -> NodeType | None:
        """Query the currently active node."""
        app_state = info.context.get("app_state")
        if not app_state:
            return None

        if app_state.active_node_id:
            node = app_state.nodes.get(app_state.active_node_id)
            if node:
                return NodeType.from_nodeinfo(node)
        return None


Query = QueryNodes
