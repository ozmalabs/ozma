# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Seat/node ownership, sharing, and permission checks.

Permission hierarchy: use < manage < admin < owner

- "use"    — connect, keyboard/mouse/audio. Cannot reconfigure.
- "manage" — also change seat profile, launch/stop games, encoder settings.
- "admin"  — full control including reshare, but cannot destroy.
- "owner"  — full control including destroy.

The machine owner (parent node owner) always has full control over all
seats on their machine.

Empty owner_id means "unowned" — no restrictions apply (backward compat).
"""

from __future__ import annotations

import logging
import time
from dataclasses import dataclass, field
from typing import Any

from state import AppState, NodeInfo

log = logging.getLogger("ozma.permissions")

# Permission level ordering (higher index = more privilege)
PERMISSION_LEVELS = ("use", "manage", "admin", "owner")
_LEVEL_INDEX = {level: i for i, level in enumerate(PERMISSION_LEVELS)}


def check_node_permission(node: NodeInfo, user_id: str, required: str,
                          state: AppState | None = None) -> bool:
    """Check if user has required permission on this node.

    required: "use", "manage", "admin", "owner"

    Returns True if:
    - node is unowned (owner_id is empty — backward compat, no restrictions)
    - user is the owner (always has all permissions)
    - user is the parent machine owner (for seats)
    - user is shared with >= required permission level
    """
    if not node.owner_id:
        # Unowned node — no restrictions
        return True

    if required not in _LEVEL_INDEX:
        return False

    required_idx = _LEVEL_INDEX[required]

    # Owner always has all permissions
    if user_id == node.owner_id:
        return True

    # Parent machine owner has full control over all seats
    if node.parent_node_id and state:
        parent = state.nodes.get(node.parent_node_id)
        if parent and parent.owner_id == user_id:
            return True

    # Check shared permissions
    if user_id in node.share_permissions:
        user_level = node.share_permissions[user_id]
        if user_level in _LEVEL_INDEX:
            return _LEVEL_INDEX[user_level] >= required_idx

    return False


def get_user_permission_level(node: NodeInfo, user_id: str,
                              state: AppState | None = None) -> str | None:
    """Get the effective permission level a user has on a node.

    Returns None if the user has no access, or one of:
    "use", "manage", "admin", "owner"
    """
    if not node.owner_id:
        # Unowned — everyone effectively has owner-level access
        return "owner"

    if user_id == node.owner_id:
        return "owner"

    # Parent machine owner
    if node.parent_node_id and state:
        parent = state.nodes.get(node.parent_node_id)
        if parent and parent.owner_id == user_id:
            return "owner"

    return node.share_permissions.get(user_id)


# ── Destructive action warnings ──────────────────────────────────────────────

@dataclass
class DestructiveActionWarning:
    action: str              # "destroy_seat", "reduce_seats", "revoke_share"
    affected_seat: str       # seat/node ID
    owner: str | None        # owner user ID if owned
    shared_users: list[str]  # users who will lose access
    message: str             # human-readable warning


def check_destructive_warnings(
    state: AppState, node_id: str, action: str,
    target_seat_count: int | None = None,
) -> list[DestructiveActionWarning]:
    """Check if an action would affect owned/shared seats.

    For "reduce_seats": checks which seats (highest index first) would be
    destroyed and whether they have owners/shares.

    For "destroy_node": checks the node itself.
    """
    warnings: list[DestructiveActionWarning] = []
    node = state.nodes.get(node_id)
    if not node:
        return warnings

    if action == "reduce_seats" and target_seat_count is not None:
        # Find seat nodes that are children of this machine
        seat_nodes = _get_child_seats(state, node_id)
        # Sort by index descending (destroy highest first)
        seat_nodes.sort(key=lambda n: n.id, reverse=True)

        # How many seats will be removed
        current_count = node.seat_count
        if target_seat_count >= current_count:
            return warnings

        seats_to_remove = current_count - target_seat_count
        affected = seat_nodes[:seats_to_remove]

        for seat_node in affected:
            if seat_node.owner_id or seat_node.shared_with:
                shared = list(seat_node.shared_with)
                parts = []
                if seat_node.owner_id:
                    parts.append(f"owned by {seat_node.owner_id}")
                if shared:
                    parts.append(f"shared with {len(shared)} user(s)")
                msg = f"Seat '{seat_node.id}' is {' and '.join(parts)}"
                warnings.append(DestructiveActionWarning(
                    action="destroy_seat",
                    affected_seat=seat_node.id,
                    owner=seat_node.owner_id or None,
                    shared_users=shared,
                    message=msg,
                ))

        # Also check if the machine itself loses seats that aren't registered
        # as separate nodes (common when seats haven't registered yet)
        if not warnings and (node.owner_id or node.shared_with):
            # The machine itself has ownership — warn about seat reduction
            shared = list(node.shared_with)
            parts = []
            if node.owner_id:
                parts.append(f"owned by {node.owner_id}")
            if shared:
                parts.append(f"shared with {len(shared)} user(s)")
            msg = f"Machine '{node_id}' is {' and '.join(parts)}, reducing from {current_count} to {target_seat_count} seats"
            warnings.append(DestructiveActionWarning(
                action="reduce_seats",
                affected_seat=node_id,
                owner=node.owner_id or None,
                shared_users=shared,
                message=msg,
            ))

    elif action == "destroy_node":
        if node.owner_id or node.shared_with:
            shared = list(node.shared_with)
            parts = []
            if node.owner_id:
                parts.append(f"owned by {node.owner_id}")
            if shared:
                parts.append(f"shared with {len(shared)} user(s)")
            msg = f"Node '{node_id}' is {' and '.join(parts)}"
            warnings.append(DestructiveActionWarning(
                action="destroy_node",
                affected_seat=node_id,
                owner=node.owner_id or None,
                shared_users=shared,
                message=msg,
            ))

        # Also check child seats
        for child in _get_child_seats(state, node_id):
            if child.owner_id or child.shared_with:
                shared = list(child.shared_with)
                parts = []
                if child.owner_id:
                    parts.append(f"owned by {child.owner_id}")
                if shared:
                    parts.append(f"shared with {len(shared)} user(s)")
                msg = f"Seat '{child.id}' is {' and '.join(parts)}"
                warnings.append(DestructiveActionWarning(
                    action="destroy_seat",
                    affected_seat=child.id,
                    owner=child.owner_id or None,
                    shared_users=shared,
                    message=msg,
                ))

    return warnings


def warnings_to_dict(warnings: list[DestructiveActionWarning]) -> list[dict[str, Any]]:
    """Serialize warnings for JSON API response."""
    return [
        {
            "action": w.action,
            "seat": w.affected_seat,
            "owner": w.owner,
            "shared_with": w.shared_users,
            "message": w.message,
        }
        for w in warnings
    ]


def _get_child_seats(state: AppState, parent_id: str) -> list[NodeInfo]:
    """Get all seat nodes that are children of a given machine."""
    return [
        n for n in state.nodes.values()
        if n.parent_node_id == parent_id
    ]


# ── User seat view ────────────────────────────────────────────────────────────

def get_user_seats(state: AppState, user_id: str) -> dict[str, list[dict[str, Any]]]:
    """Get all seats relevant to a user: owned + shared.

    Returns {"owned": [...], "shared": [...]}.
    """
    owned: list[dict[str, Any]] = []
    shared: list[dict[str, Any]] = []

    for node in state.nodes.values():
        if node.owner_id == user_id:
            owned.append({
                "id": node.id,
                "machine": node.parent_node_id or node.id,
                "status": "online",
                "permission": "owner",
            })
        elif user_id in node.share_permissions:
            perm = node.share_permissions[user_id]
            shared.append({
                "id": node.id,
                "machine": node.parent_node_id or node.id,
                "owner": node.owner_id,
                "permission": perm,
                "status": "online",
            })

    return {"owned": owned, "shared": shared}
