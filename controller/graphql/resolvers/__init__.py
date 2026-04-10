# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
GraphQL resolvers package.
"""

from .nodes import QueryNodes
from .scenarios import QueryScenarios

__all__ = [
    "QueryNodes",
    "QueryScenarios",
]
