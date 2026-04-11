# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Resolvers for scenario-related GraphQL queries.
"""

from typing import Optional

import strawberry
from strawberry.types import Info

from controller.scenarios import ScenarioManager


@strawberry.type
class QueryScenarios:
    """Query resolvers for scenarios."""

    @strawberry.field
    def scenarios(self, info: Info) -> list["ScenarioType"]:
        """Query all scenarios."""
        scenario_manager = info.context.get("scenario_manager")
        if not scenario_manager:
            return []

        scenarios_list = []
        for scenario_id in sorted(scenario_manager._scenarios.keys()):
            scenario = scenario_manager._scenarios[scenario_id]
            from controller.graphql.types.scenarios import ScenarioType

            scenarios_list.append(ScenarioType.from_scenario(scenario))
        return scenarios_list

    @strawberry.field
    def scenario(self, info: Info, id: str) -> Optional["ScenarioType"]:
        """Query a single scenario by ID."""
        scenario_manager = info.context.get("scenario_manager")
        if not scenario_manager:
            return None

        scenario = scenario_manager._scenarios.get(id)
        if not scenario:
            return None

        from controller.graphql.types.scenarios import ScenarioType

        return ScenarioType.from_scenario(scenario)

    @strawberry.field
    def active_scenario(self, info: Info) -> Optional["ScenarioType"]:
        """Query the currently active scenario."""
        scenario_manager = info.context.get("scenario_manager")
        if not scenario_manager:
            return None

        active_id = scenario_manager.active_id
        if not active_id:
            return None

        scenario = scenario_manager._scenarios.get(active_id)
        if not scenario:
            return None

        from controller.graphql.types.scenarios import ScenarioType

        return ScenarioType.from_scenario(scenario)


# Import ScenarioType after class definition to avoid circular import
from controller.graphql.types.scenarios import ScenarioType
