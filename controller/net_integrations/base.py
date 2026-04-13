# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Shared dataclasses and Protocol for network-hardware backends.

All backend adapters (UniFi, MikroTik, Omada, OpenWrt, pfSense …) implement
``NetworkBackend``.  The dataclasses here are the only types that cross the
boundary between ``IoTNetworkManager`` and a backend — backends must not leak
vendor-specific types into the manager.

Stdlib + typing only; no third-party imports.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from enum import Enum
from typing import Protocol, runtime_checkable


# ── Enums ─────────────────────────────────────────────────────────────────────

class PortMode(str, Enum):
    ACCESS = "access"   # untagged on one VLAN, drops all others
    TRUNK  = "trunk"    # tagged on multiple VLANs, native VLAN untagged
    HYBRID = "hybrid"   # untagged on one VLAN, tagged on others (Omada term)


# ── Request / spec dataclasses ────────────────────────────────────────────────

@dataclass
class VLANSpec:
    """Desired state for a single VLAN — passed to ``ensure_vlan``."""

    vlan_id:      int
    name:         str
    subnet:       str           # e.g. "10.20.0.0/24"
    gateway:      str           # e.g. "10.20.0.1"
    dhcp_enabled: bool = True
    dhcp_start:   str  = ""
    dhcp_end:     str  = ""
    purpose:      str  = ""     # "management" | "mesh" | "iot" | ""


@dataclass
class PortSpec:
    """Desired port configuration — passed to ``assign_port``."""

    port_id:      str           # e.g. "ether3", "Port 3", "1/0/3"
    mode:         PortMode
    native_vlan:  int           # untagged / PVID
    tagged_vlans: list[int] = field(default_factory=list)
    poe_enabled:  bool | None  = None
    description:  str          = ""


# ── Read / topology dataclasses ───────────────────────────────────────────────

@dataclass
class SwitchPort:
    """Current state of a single switch port, as reported by the device."""

    port_id:            str
    mode:               PortMode | None
    native_vlan:        int | None
    tagged_vlans:       list[int] = field(default_factory=list)
    connected_mac:      str  = ""
    connected_hostname: str  = ""
    link_up:            bool = False
    speed_mbps:         int  = 0


@dataclass
class NetworkVLAN:
    """A VLAN as it currently exists on the managed device."""

    vlan_id:      int
    name:         str
    subnet:       str  = ""
    gateway:      str  = ""
    dhcp_enabled: bool = False


@dataclass
class NetworkDevice:
    """A managed switch / AP / router discovered via the backend API."""

    device_id: str
    name:      str
    model:     str = ""
    ip:        str = ""
    ports:     list[SwitchPort] = field(default_factory=list)


@dataclass
class DHCPLease:
    """A single DHCP lease entry."""

    mac:       str
    ip:        str
    hostname:  str  = ""
    expires:   str  = ""     # ISO-8601 or epoch string; empty = unknown
    is_static: bool = False


@dataclass
class NetworkTopology:
    """Full snapshot of the managed network as seen by the backend."""

    vlans:   list[NetworkVLAN]   = field(default_factory=list)
    devices: list[NetworkDevice] = field(default_factory=list)
    leases:  list[DHCPLease]     = field(default_factory=list)


# ── Result dataclass ──────────────────────────────────────────────────────────

@dataclass
class ProvisionResult:
    """
    Outcome of a mutating backend operation (``ensure_vlan``, ``assign_port``).

    ``success`` is False only when the operation could not be completed at all.
    Individual items that were already correct are listed in ``skipped``; items
    that were changed are listed in ``changes``; non-fatal warnings go in
    ``errors`` (``success`` may still be True).
    """

    success:  bool
    changes:  list[str] = field(default_factory=list)  # what was applied
    errors:   list[str] = field(default_factory=list)  # warnings / non-fatal errors
    skipped:  list[str] = field(default_factory=list)  # already correct, no-op


# ── Backend Protocol ──────────────────────────────────────────────────────────

@runtime_checkable
class NetworkBackend(Protocol):
    """
    Contract that every network-hardware adapter must satisfy.

    Implementations live in ``controller/net_integrations/<vendor>.py`` and are
    selected at runtime by ``IoTNetworkManager.configure_backend()``.

    All methods are async; callers must ``await`` them.
    """

    #: Short identifier used in logs and config (e.g. "unifi", "mikrotik").
    name: str

    async def get_topology(self) -> NetworkTopology:
        """
        Return a full snapshot of VLANs, devices, and DHCP leases.

        Should not raise — return an empty ``NetworkTopology`` on failure and
        log the error internally.
        """
        ...

    async def ensure_vlan(self, spec: VLANSpec) -> ProvisionResult:
        """
        Idempotently create or update a VLAN to match *spec*.

        If the VLAN already exists and matches, return a result with the item
        in ``skipped``.  If it needs to be created or updated, apply the change
        and list it in ``changes``.
        """
        ...

    async def assign_port(
        self,
        device_id:    str,
        port_id:      str,
        mode:         PortMode,
        native_vlan:  int,
        tagged_vlans: list[int],
    ) -> ProvisionResult:
        """
        Configure a switch port's VLAN membership.

        *device_id* identifies the managed switch (matches ``NetworkDevice.device_id``).
        *port_id* identifies the port on that switch (matches ``SwitchPort.port_id``).
        """
        ...

    async def get_dhcp_leases(self) -> list[DHCPLease]:
        """Return all current DHCP leases visible to this backend."""
        ...

    async def set_dhcp_reservation(self, mac: str, ip: str, hostname: str) -> bool:
        """
        Create or update a static DHCP reservation.

        Returns True on success, False on failure (log the error internally).
        """
        ...
