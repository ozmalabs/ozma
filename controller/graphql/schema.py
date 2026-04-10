# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Root GraphQL schema for Ozma Controller."""

import strawberry

from .types.nodes import NodeType
from .types.scenarios import ScenarioType, BindingType
from .resolvers.nodes import QueryNodes
from .resolvers.scenarios import QueryScenarios


@strawberry.type
class Query(QueryNodes, QueryScenarios):
    """Root query type combining all GraphQL queries."""

    pass


# Root Schema
schema = strawberry.Schema(query=Query)
