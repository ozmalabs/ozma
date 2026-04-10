# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Root GraphQL schema definition for Ozma Controller API.
"""

import strawberry

from controller.graphql.resolvers.nodes import QueryNodes
from controller.graphql.resolvers.scenarios import QueryScenarios
from controller.graphql.types.nodes import NodeType
from controller.graphql.types.scenarios import ScenarioType


@strawberry.type
class Query(QueryNodes, QueryScenarios):
    """Root query type combining all query resolvers."""


# Create the schema
schema = strawberry.Schema(query=Query)

__all__ = ["schema", "Query"]
