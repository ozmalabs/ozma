# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""GraphQL resolvers for Scenario queries."""

from typing import Any

import strawberry

from ..types.scenarios import BindingType, ScenarioType
from ..scenarios import ScenarioManager


@strawberry.type
class QueryScenarios:
    """Query resolvers for scenario-related operations."""

    @strawberry.field
    def scenarios(self, info: strawberry.Info) -> list[ScenarioType]:
        """Query all scenarios."""
        scenario_mgr = info.context.get("scenario_manager")
        if not scenario_mgr:
            return []

        return [ScenarioType.from_scenario(s) for s in scenario_mgr._scenarios.values()]

    @strawberry.field
    def scenario(self, info: strawberry.Info, id: str) -> ScenarioType | None:
        """Query a single scenario by ID."""
        scenario_mgr = info.context.get("scenario_manager")
        if not scenario_mgr:
            return None

        scenario = scenario_mgr._scenarios.get(id)
        if scenario:
            return ScenarioType.from_scenario(scenario)
        return None

    @strawberry.field
    def active_scenario(self, info: strawberry.Info) -> ScenarioType | None:
        """Query the currently active scenario."""
        scenario_mgr = info.context.get("scenario_manager")
        if not scenario_mgr:
            return None

        active_id = scenario_mgr.active_id
        if active_id:
            scenario = scenario_mgr._scenarios.get(active_id)
            if scenario:
                return ScenarioType.from_scenario(scenario)
        return None

    @strawberry.field
    def bindings(self, info: strawberry.Info) -> list[BindingType]:
        """Query all bindings (scenario-to-node mappings)."""
        scenario_mgr = info.context.get("scenario_manager")
        if not scenario_mgr:
            return []

        active_id = scenario_mgr.active_id
        bindings = []
        for scenario in scenario_mgr._scenarios.values():
            bindings.append(BindingType.from_scenario(
                scenario,
                active=(scenario.id == active_id)
            ))
        return bindings


Query = QueryScenarios
