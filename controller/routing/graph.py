# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
RoutingGraph — the in-memory routing graph.

Holds the set of devices, ports, and links discovered so far.
Phase 1: observational only. All mutations come from the GraphBuilder.

The graph is NOT persisted. It is rebuilt from discovery on each startup
(per docs/routing/implementation.md §2).
"""

from __future__ import annotations

import logging
from typing import Iterator

from .model import (
    Device, Port, Link, DeviceType, MediaType, PortRef, PortDirection,
)

log = logging.getLogger("ozma.routing.graph")


class RoutingGraph:
    """
    In-memory routing graph.

    All operations are synchronous and O(1) or O(n) on the graph size.
    Phase 1 graphs have <50 devices and <200 links, so linear scans are fine.
    """

    def __init__(self) -> None:
        self._devices: dict[str, Device] = {}
        self._links: dict[str, Link] = {}

    # ── Mutations ────────────────────────────────────────────────────────────

    def add_device(self, device: Device) -> None:
        if device.id in self._devices:
            log.debug("Replacing existing device %s (%s)", device.id, device.name)
        self._devices[device.id] = device

    def remove_device(self, device_id: str) -> None:
        if device_id not in self._devices:
            return
        # Remove all links involving this device's ports
        to_remove = [
            lid for lid, link in self._links.items()
            if link.source.device_id == device_id or link.sink.device_id == device_id
        ]
        for lid in to_remove:
            del self._links[lid]
        del self._devices[device_id]

    def add_link(self, link: Link) -> None:
        self._links[link.id] = link
        # Update port state to reflect the connection
        src_port = self._get_port(link.source)
        snk_port = self._get_port(link.sink)
        if src_port and link.sink not in src_port.current_state.connected_to:
            src_port.current_state.connected_to.append(link.sink)
        if snk_port and link.source not in snk_port.current_state.connected_to:
            snk_port.current_state.connected_to.append(link.source)

    def remove_link(self, link_id: str) -> None:
        link = self._links.pop(link_id, None)
        if link is None:
            return
        src_port = self._get_port(link.source)
        snk_port = self._get_port(link.sink)
        if src_port:
            src_port.current_state.connected_to = [
                r for r in src_port.current_state.connected_to if r != link.sink
            ]
        if snk_port:
            snk_port.current_state.connected_to = [
                r for r in snk_port.current_state.connected_to if r != link.source
            ]

    def clear(self) -> None:
        self._devices.clear()
        self._links.clear()

    # ── Queries ──────────────────────────────────────────────────────────────

    def get_device(self, device_id: str) -> Device | None:
        return self._devices.get(device_id)

    def get_link(self, link_id: str) -> Link | None:
        return self._links.get(link_id)

    def devices(self) -> Iterator[Device]:
        yield from self._devices.values()

    def devices_by_type(self, *types: DeviceType) -> list[Device]:
        return [d for d in self._devices.values() if d.type in types]

    def links(self) -> Iterator[Link]:
        yield from self._links.values()

    def links_from(self, port_ref: PortRef) -> list[Link]:
        """All links where this port is the source."""
        return [l for l in self._links.values() if l.source == port_ref]

    def links_to(self, port_ref: PortRef) -> list[Link]:
        """All links where this port is the sink."""
        return [l for l in self._links.values() if l.sink == port_ref]

    def links_involving(self, device_id: str) -> list[Link]:
        """All links touching any port on this device."""
        return [
            l for l in self._links.values()
            if l.source.device_id == device_id or l.sink.device_id == device_id
        ]

    @property
    def device_count(self) -> int:
        return len(self._devices)

    @property
    def link_count(self) -> int:
        return len(self._links)

    # ── Serialisation ────────────────────────────────────────────────────────

    def to_dict(self) -> dict:
        return {
            "devices": [d.to_dict() for d in self._devices.values()],
            "links": [l.to_dict() for l in self._links.values()],
            "stats": {
                "device_count": self.device_count,
                "link_count": self.link_count,
            },
        }

    def __repr__(self) -> str:
        return f"RoutingGraph(devices={self.device_count}, links={self.link_count})"

    # ── Private ──────────────────────────────────────────────────────────────

    def _get_port(self, ref: PortRef) -> Port | None:
        device = self._devices.get(ref.device_id)
        if device is None:
            return None
        return device.get_port(ref.port_id)
