# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Bearer-token resolution for controller → node HTTP calls.

A node may be started with ``--api-token``. The token arrives either from the
controller's persistent per-node store (loaded into ``NodeInfo.api_token`` and
taking precedence), from registration, or from the controller-wide
``OZMA_NODE_API_TOKEN`` (``AppState.node_api_token``). Per-node tokens win; the
global token is only a fallback.

Callers use :func:`node_api_headers` / :func:`node_api_headers_for_state` to
build request headers. Tokens are never logged, serialized, or returned in an
API response; these helpers only hand them to an HTTP client.
"""

from __future__ import annotations

from typing import Any


def node_api_token(node: Any | None, fallback: str = "") -> str:
    """Resolve a node's bearer token, preferring its registration token.

    ``fallback`` is normally the controller-wide ``OZMA_NODE_API_TOKEN``.
    Empty means the node API is unauthenticated.
    """
    token = getattr(node, "api_token", "") if node is not None else ""
    token = token or fallback or ""
    return str(token).strip()


def node_api_headers(node: Any | None = None, fallback: str = "") -> dict[str, str]:
    """Return ``Authorization`` headers for a node request, or ``{}`` when unset."""
    token = node_api_token(node, fallback)
    return {"Authorization": f"Bearer {token}"} if token else {}


def state_node_api_token(state: Any | None) -> str:
    """Return the controller-wide node token from ``AppState``, if configured."""
    if state is None:
        return ""
    return str(getattr(state, "node_api_token", "") or "").strip()


def node_api_headers_for_state(node: Any | None, state: Any | None) -> dict[str, str]:
    """Resolve a node token from an ``AppState`` and return request headers."""
    return node_api_headers(node, state_node_api_token(state))


def node_auth_error(status: int) -> str:
    """Clear, token-free message for a node 401/403 response."""
    return (
        "Node rejected the controller's bearer token "
        f"(HTTP {status}); configure OZMA_NODE_API_TOKEN to match the node"
    )
