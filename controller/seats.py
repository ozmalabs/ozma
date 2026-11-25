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

import asyncio
import json
import re
import time
import urllib.error
import urllib.request
from typing import Any, Iterable

from node_auth import node_api_headers

# Nodes that miss their re-registration window stay in state briefly while
# discovery evicts them. Report those as offline in the read-only view rather
# than pretending the seat is healthy.
DEFAULT_OFFLINE_AFTER = 180.0

# Read-only persisted seat report served by newer `ozma-node` builds.
NODE_SEATS_PATH = "/api/v1/seats"
DEFAULT_NODE_SEAT_TIMEOUT = 1.5

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


def seat_identity(node: Any, *, seat_id_override: str | None = None) -> dict[str, Any] | None:
    """Derive a seat identity for *node*, or ``None`` for ordinary nodes.

    ``seat_id_override`` is an explicit identity discovered from the node's own
    read-only ``/api/v1/seats`` document. It is used only when non-empty and has
    the same authority as an explicit ``seat_id`` field.
    """
    node_id = normalize_node_id(getattr(node, "id", "") or "")
    explicit = normalize_node_id(
        seat_id_override or getattr(node, "seat_id", "") or ""
    )
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


def _seat_entry(
    node: Any,
    identity: dict[str, Any],
    online: bool,
    extra: dict[str, Any] | None = None,
) -> dict[str, Any]:
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
    if extra:
        entry.update(extra)
    return entry


def _seat_sort_key(seat: dict[str, Any]) -> tuple[bool, int, str]:
    index = seat.get("seat_index")
    return (index is None, index if isinstance(index, int) else 0, str(seat.get("node_id", "")))


def group_seats(
    nodes: Iterable[Any],
    *,
    now: float | None = None,
    offline_after: float = DEFAULT_OFFLINE_AFTER,
    seat_id_overrides: dict[str, str] | None = None,
    node_extras: dict[str, dict[str, Any]] | None = None,
) -> dict[str, Any]:
    """Group already-registered nodes into read-only seat groups.

    ``seat_id_overrides`` and ``node_extras`` come from a node's own read-only
    ``/api/v1/seats`` report when one was present. Without them the result is
    exactly the historical name/metadata-based grouping.

    The return value is JSON-safe and never includes node API tokens or other
    secrets: only the display/network fields needed by the dashboard.
    """
    moment = time.monotonic() if now is None else now
    overrides = seat_id_overrides or {}
    extras = node_extras or {}
    grouped: dict[str, dict[str, Any]] = {}
    unseated: list[str] = []

    for node in nodes:
        node_id = getattr(node, "id", "")
        identity = seat_identity(node, seat_id_override=overrides.get(node_id))
        if identity is None:
            unseated.append(node_id)
            continue
        machine = identity["machine"] or "unknown"
        group = grouped.setdefault(machine, {"machine": machine, "seats": []})
        group["seats"].append(
            _seat_entry(
                node,
                identity,
                _is_online(node, moment, offline_after),
                extras.get(node_id),
            )
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


def _typed_node_error(payload: Any, status: int) -> dict[str, Any]:
    """Preserve a node's typed error body instead of flattening it to 502."""
    if isinstance(payload, dict):
        error = payload.get("error")
        if isinstance(error, dict):
            code = str(error.get("code") or "node_error")
            message = str(error.get("message") or "")
            return {"code": code, "message": message}
        if isinstance(error, str) and error.strip():
            return {"code": "node_error", "message": error.strip()}
        detail = payload.get("detail")
        if isinstance(detail, str) and detail.strip():
            return {"code": "node_error", "message": detail.strip()}
    if status:
        return {"code": "node_error", "message": f"node returned HTTP {status}"}
    return {"code": "node_unreachable", "message": "node seat report unavailable"}


async def fetch_node_seat_report(
    node: Any,
    *,
    fallback_token: str = "",
    timeout: float = DEFAULT_NODE_SEAT_TIMEOUT,
) -> tuple[int, Any]:
    """Fetch a node's read-only persisted seat report.

    ``(0, None)`` means the node has no HTTP API, so the caller keeps the
    historical grouping with no error. A reachable node's typed HTTP body is
    returned unchanged with its status. A transport failure returns
    ``(0, {"error": ...})`` so it can be reported without dropping the node.
    """
    api_port = getattr(node, "api_port", None)
    host = (getattr(node, "host", "") or "").strip()
    if not api_port or not host:
        return (0, None)
    url = f"http://{host}:{api_port}{NODE_SEATS_PATH}"
    headers = node_api_headers(node, fallback_token)

    def _fetch() -> tuple[int, Any]:
        request = urllib.request.Request(url, headers=headers)
        try:
            with urllib.request.urlopen(request, timeout=timeout) as response:
                body = response.read()
                try:
                    return response.status, json.loads(body or b"null")
                except ValueError:
                    return response.status, body.decode("utf-8", "replace")
        except urllib.error.HTTPError as error:
            body = error.read()
            try:
                payload = json.loads(body)
            except ValueError:
                payload = body.decode("utf-8", "replace")
            return error.code, payload
        except Exception as error:  # noqa: BLE001 - typed below, never an HTTPException
            return 0, {
                "ok": False,
                "error": {"code": "node_unreachable", "message": str(error)},
            }

    loop = asyncio.get_running_loop()
    return await loop.run_in_executor(None, _fetch)


def _node_seat_override(report: Any) -> tuple[str, dict[str, Any] | None]:
    """Return ``(seat_id, seat_document)`` from a successful node report."""
    if not isinstance(report, dict) or report.get("configured") is not True:
        return ("", None)
    seat = report.get("node_seat")
    if not isinstance(seat, dict):
        seat = None
    seat_id = ""
    if seat is not None:
        seat_id = normalize_node_id(seat.get("seat_id"))
    if not seat_id:
        seat_id = normalize_node_id(report.get("seat_id"))
    return (seat_id, seat)


def _multiseat_extra(report: dict[str, Any], seat: dict[str, Any] | None) -> dict[str, Any]:
    status = report.get("status")
    plan_seat_count = None
    if isinstance(status, dict) and isinstance(status.get("seat_count"), int):
        plan_seat_count = status["seat_count"]
    extra: dict[str, Any] = {
        "mode": report.get("mode") if isinstance(report.get("mode"), str) else "read-only",
        "plan_seat_count": plan_seat_count,
    }
    if seat is not None:
        if isinstance(seat.get("state"), str):
            extra["state"] = seat["state"]
        if isinstance(seat.get("available"), bool):
            extra["available"] = seat["available"]
    return extra


async def group_seats_with_node_reports(
    nodes: Iterable[Any],
    fetch_report: Any = fetch_node_seat_report,
    *,
    now: float | None = None,
    offline_after: float = DEFAULT_OFFLINE_AFTER,
    fallback_token: str = "",
    timeout: float = DEFAULT_NODE_SEAT_TIMEOUT,
) -> dict[str, Any]:
    """Group seats, consuming each node's read-only ``/api/v1/seats`` report.

    A node that does not serve the route (404), has no HTTP API, or answered
    ``configured:false`` is grouped exactly as before. A successful report can
    supply an explicit seat id, so a renamed node still joins its machine group.
    A node's typed error is passed through in ``node_errors`` and that node still
    falls back to the historical grouping; no node is dropped and no seat is
    invented.
    """
    node_list = list(nodes)
    if not node_list:
        return group_seats(
            node_list, now=now, offline_after=offline_after
        )

    results = await asyncio.gather(
        *(
            fetch_report(node, fallback_token=fallback_token, timeout=timeout)
            for node in node_list
        ),
        return_exceptions=True,
    )

    overrides: dict[str, str] = {}
    extras: dict[str, dict[str, Any]] = {}
    errors: list[dict[str, Any]] = []

    for node, result in zip(node_list, results):
        node_id = getattr(node, "id", "")
        if isinstance(result, Exception):
            errors.append(
                {
                    "node_id": node_id,
                    "status": 0,
                    "error": {
                        "code": "node_unreachable",
                        "message": str(result),
                    },
                }
            )
            continue
        status, payload = result
        if status == 404:
            continue
        if status == 0 and payload is None:
            continue
        if status >= 400 or status == 0:
            errors.append(
                {
                    "node_id": node_id,
                    "status": status,
                    "error": _typed_node_error(payload, status),
                }
            )
            continue
        if not isinstance(payload, dict):
            errors.append(
                {
                    "node_id": node_id,
                    "status": status,
                    "error": {
                        "code": "seat_report_invalid",
                        "message": "node /api/v1/seats did not return an object",
                    },
                }
            )
            continue
        if payload.get("configured") is not True:
            continue
        seat_id, seat = _node_seat_override(payload)
        if seat_id:
            overrides[node_id] = seat_id
        extras[node_id] = {"multiseat": _multiseat_extra(payload, seat)}

    result = group_seats(
        node_list,
        now=now,
        offline_after=offline_after,
        seat_id_overrides=overrides,
        node_extras=extras,
    )
    if errors:
        result["node_errors"] = sorted(errors, key=lambda item: str(item["node_id"]))
    return result
