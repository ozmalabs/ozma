# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Root GraphQL schema for Ozma Controller."""

import strawberry

from .types.nodes import NodeType
from .types.scenarios import BindingType, ScenarioType
from .resolvers.nodes import Query as QueryNodes
from .resolvers.scenarios import Query as QueryScenarios


@strawberry.type
class Query(QueryNodes, QueryScenarios):
    """Root query type combining all GraphQL queries."""

    @strawberry.field
    def bindings(self, info: strawberry.Info) -> list[BindingType]:
        """Query all bindings (scenario-to-node mappings)."""
        scenario_mgr = info.context.get("scenario_manager")
        if not scenario_mgr:
            return []

        active_id = scenario_mgr.active_id
        bindings = []
        for scenario in scenario_mgr.list():
            bindings.append(BindingType(
                id=scenario["id"],
                scenario_id=scenario["id"],
                node_id=scenario["node_id"],
                active=(scenario["id"] == active_id)
            ))
        return bindings


# Root Schema
schema = strawberry.Schema(query=Query)
