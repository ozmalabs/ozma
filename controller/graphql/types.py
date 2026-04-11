# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
GraphQL type definitions for Ozma Controller.

This module provides backwards-compatible type definitions.
The main type definitions are now in subscriptions.py to ensure
proper integration with async generator subscriptions.

For new code, import types from subscriptions.py instead.
"""

from .subscriptions import (
    NodeType,
    ScenarioType,
    AlertType,
    AudioLevelType,
    SnapshotType,
)

__all__ = [
    "NodeType",
    "ScenarioType",
    "AlertType",
    "AudioLevelType",
    "SnapshotType",
]
