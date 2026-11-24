# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Read-only multi-seat grouping for controller nodes.

``ozma-multiseat`` launches one ``ozma-node`` child per seat and names it
``<machine>-seat-<n>`` (also exported as ``OZMA_SEAT_ID``). The controller
still registers each seat as an independent node, but operators need to see
which nodes belong to the same physical machine.

This module derives that grouping. It never changes registration or lifecycle
state; it only reads the nodes already known to ``AppState``.

A node is treated as a seat when any of these is true:

1. it carries an explicit ``seat_id`` (direct registration or node metadata),
2. its node id (or explicit ``seat_id``) follows ``<machine>-seat-<n>``, or
3. it carries a ``parent_node_id`` naming the machine it belongs to.

Nodes that match none of these rules stay ungrouped, so ordinary nodes are
never reported as seats.
"""

from __future__ import annotations

import re
import time
from typing import Any, Iterable

# Nodes that miss their re-registration window stay in state briefly while
# discovery evicts them. Report those as offline in the read-only view rather
# than pretending the seat is healthy.
DEFAULT_OFFLINE_AFTER = 180.0

_MDNS_SUFFIXES = (
    "._ozma._udp.local.",
    "._ozma._udp.local",
    "._ozma._tcp.local.",
    "._ozma._tcp.local",
)

# ``<machine>-seat-<n>``. The machine prefix must be non-empty so a bare
# ``seat-0`` is not invented as a seat.
_SEAT_PATTERN = re.compile(r"^(?P<machine>.+)-seat-(?P<index>\d+)$")


def normalize_node_id(node_id: str | None) -> str:
    """Return the short, human-facing form of an mDNS node id."""
    value = (node_id or "").strip()
    for suffix in _MDNS_SUFFIXES:
        if value.endswith(suffix):
            value = value[: -len(suffix)]
            break
    return value.rstrip(".")


def parse_seat_id(value: str | None) -> tuple[str, int] | None:
    """Split ``<machine>-seat-<n>`` into ``(machine, index)``.

    Returns ``None`` for ids that do not follow the convention.
    """
    match = _SEAT_PATTERN.match(normalize_node_id(value))
    if match is None:
        return None
    return match.group("machine"), int(match.group("index"))


def _parent_machine(node: Any) -> str:
    return normalize_node_id(getattr(node, "parent_node_id", "") or "")


def seat_identity(node: Any) -> dict[str, Any] | None:
    """Derive a seat identity for *node*, or ``None`` for ordinary nodes."""
    node_id = normalize_node_id(getattr(node, "id", "") or "")
    explicit = normalize_node_id(getattr(node, "seat_id", "") or "")
    parent = _parent_machine(node)

    # An explicit seat id is authoritative membership. Parse it for the
    # machine/index when it follows the convention; otherwise keep the id and
    # let parent_node_id (when present) name the machine.
    if explicit:
        parsed = parse_seat_id(explicit)
        if parsed is not None:
            machine, index = parsed
            return {
                "seat_id": explicit,
                "machine": parent or machine,
                "seat_index": index,
            }
        return {
            "seat_id": explicit,
            "machine": parent or node_id or explicit,
            "seat_index": None,
        }

    # No explicit field: fall back to the name convention used by
    # ozma-multiseat. parent_node_id overrides the machine prefix so an
    # operator can rename a seat node without splitting its group.
    parsed = parse_seat_id(node_id)
    if parsed is not None:
        machine, index = parsed
        return {
            "seat_id": node_id,
            "machine": parent or machine,
            "seat_index": index,
        }

    # A parent link alone marks this node as a seat even if its name does not
    # use the convention. The index is unknown (None) rather than fabricated.
    if parent:
        return {
            "seat_id": node_id,
            "machine": parent,
            "seat_index": None,
        }

    return None


def _display_outputs(node: Any) -> list[dict[str, Any]]:
    outputs = getattr(node, "display_outputs", None) or []
    return [dict(output) for output in outputs if isinstance(output, dict)]


def _displays(outputs: list[dict[str, Any]]) -> list[dict[str, Any]]:
    return [
        {
            "index": index,
            "width": output.get("width", 0),
            "height": output.get("height", 0),
            "name": output.get("name", f"display{index}"),
        }
        for index, output in enumerate(outputs)
    ]


def _is_online(node: Any, now: float, offline_after: float) -> bool:
    try:
        last_seen = float(getattr(node, "last_seen", 0.0) or 0.0)
    except (TypeError, ValueError):
        return True
    if last_seen <= 0.0:
        return True
    return (now - last_seen) <= offline_after


def _seat_entry(node: Any, identity: dict[str, Any], online: bool) -> dict[str, Any]:
    outputs = _display_outputs(node)
    entry: dict[str, Any] = {
        "node_id": getattr(node, "id", ""),
        "seat_id": identity["seat_id"],
        "seat_index": identity["seat_index"],
        "machine": identity["machine"],
        "online": online,
        "host": getattr(node, "host", ""),
        "port": getattr(node, "port", None),
        "api_port": getattr(node, "api_port", None),
        "stream_port": getattr(node, "stream_port", None),
        "machine_class": getattr(node, "machine_class", ""),
        "role": getattr(node, "role", ""),
        "hw": getattr(node, "hw", ""),
        "fw_version": getattr(node, "fw_version", ""),
        "display_outputs": outputs,
        "displays": _displays(outputs),
    }
    vnc_host = getattr(node, "vnc_host", None)
    vnc_port = getattr(node, "vnc_port", None)
    if vnc_host:
        entry["vnc_host"] = vnc_host
    if vnc_port:
        entry["vnc_port"] = vnc_port
    parent = getattr(node, "parent_node_id", "") or ""
    if parent:
        entry["parent_node_id"] = parent
    return entry


def _seat_sort_key(seat: dict[str, Any]) -> tuple[bool, int, str]:
    index = seat.get("seat_index")
    return (index is None, index if isinstance(index, int) else 0, str(seat.get("node_id", "")))


def group_seats(
    nodes: Iterable[Any],
    *,
    now: float | None = None,
    offline_after: float = DEFAULT_OFFLINE_AFTER,
) -> dict[str, Any]:
    """Group already-registered nodes into read-only seat groups.

    The return value is JSON-safe and never includes node API tokens or other
    secrets: only the display/network fields needed by the dashboard.
    """
    moment = time.monotonic() if now is None else now
    grouped: dict[str, dict[str, Any]] = {}
    unseated: list[str] = []

    for node in nodes:
        identity = seat_identity(node)
        if identity is None:
            unseated.append(getattr(node, "id", ""))
            continue
        machine = identity["machine"] or "unknown"
        group = grouped.setdefault(machine, {"machine": machine, "seats": []})
        group["seats"].append(
            _seat_entry(node, identity, _is_online(node, moment, offline_after))
        )

    groups: list[dict[str, Any]] = []
    for machine in sorted(grouped, key=str.casefold):
        group = grouped[machine]
        seats = sorted(group["seats"], key=_seat_sort_key)
        online_count = sum(1 for seat in seats if seat["online"])
        groups.append(
            {
                "machine": machine,
                "host": machine,
                "node_count": len(seats),
                "seat_count": len(seats),
                "online_count": online_count,
                "offline_count": len(seats) - online_count,
                "online": online_count > 0,
                "all_online": online_count == len(seats),
                "seats": seats,
            }
        )

    total_seats = sum(group["seat_count"] for group in groups)
    total_online = sum(group["online_count"] for group in groups)
    return {
        "groups": groups,
        "seat_count": total_seats,
        "online_count": total_online,
        "offline_count": total_seats - total_online,
        "unseated_node_ids": sorted(unseated),
    }
