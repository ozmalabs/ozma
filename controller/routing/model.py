# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Core data model for the Ozma routing graph.

Implements the four graph primitives from docs/routing/graph-primitives.md
and the information quality model from docs/routing/quality.md.

Phase 1: observational model only. All link metrics are `spec` or `assumed`
quality until active measurement (Phase 5) is added.
"""

from __future__ import annotations

import time
from dataclasses import dataclass, field
from enum import Enum
from typing import Any


# ── Information quality ───────────────────────────────────────────────────────

class InfoQuality(str, Enum):
    """
    Provenance tag for every measured or reported property.

    Trust ordering: user > measured > inferred > reported > commanded > spec > assumed
    """
    user = "user"           # explicitly set by the user
    measured = "measured"   # from active probing or passive measurement
    inferred = "inferred"   # derived from measured data + known parameters
    reported = "reported"   # from OS/driver API (lsusb, PipeWire, etc.)
    commanded = "commanded" # sent a command but cannot confirm it was applied
    spec = "spec"           # from device spec or standard (USB 3.0 = 5 Gbps)
    assumed = "assumed"     # heuristic or default

    @property
    def trust_level(self) -> int:
        """Higher is more trusted."""
        return {
            "user": 6, "measured": 5, "inferred": 4,
            "reported": 3, "commanded": 2, "spec": 1, "assumed": 0,
        }[self.value]

    def __lt__(self, other: "InfoQuality") -> bool:
        return self.trust_level < other.trust_level

    def __le__(self, other: "InfoQuality") -> bool:
        return self.trust_level <= other.trust_level

    def __gt__(self, other: "InfoQuality") -> bool:
        return self.trust_level > other.trust_level

    def __ge__(self, other: "InfoQuality") -> bool:
        return self.trust_level >= other.trust_level


# ── Device type / media type enums ────────────────────────────────────────────

class DeviceType(str, Enum):
    controller = "controller"
    node = "node"
    target = "target"
    capture_card = "capture_card"
    display = "display"
    screen = "screen"
    audio_interface = "audio_interface"
    audio_processor = "audio_processor"
    usb_hub = "usb_hub"
    usb_controller = "usb_controller"
    network_interface = "network_interface"
    dock = "dock"
    codec = "codec"
    software_codec = "software_codec"
    peripheral = "peripheral"
    control_surface = "control_surface"
    rgb = "rgb"
    camera = "camera"
    speaker = "speaker"
    microphone = "microphone"
    phone = "phone"
    actuator = "actuator"
    sensor = "sensor"
    ups = "ups"
    pdu = "pdu"
    power_strip = "power_strip"
    lock = "lock"
    hvac = "hvac"
    lighting = "lighting"
    occupancy = "occupancy"
    relay = "relay"
    switch = "switch"
    vm_host = "vm_host"
    service = "service"
    media_receiver = "media_receiver"
    media_source = "media_source"
    notification_sink = "notification_sink"
    metrics_sink = "metrics_sink"
    furniture = "furniture"
    network_switch = "network_switch"
    router = "router"
    access_point = "access_point"
    avr = "avr"
    virtual = "virtual"
    sip_endpoint = "sip_endpoint"
    pbx = "pbx"


class MediaType(str, Enum):
    video = "video"
    audio = "audio"
    hid = "hid"
    screen = "screen"
    rgb = "rgb"
    control = "control"
    data = "data"
    power = "power"
    mixed = "mixed"


class PortDirection(str, Enum):
    source = "source"   # produces data
    sink = "sink"       # consumes data


class LinkStatus(str, Enum):
    active = "active"       # data flowing
    warm = "warm"           # ready, no data flowing
    standby = "standby"     # not initialised
    failed = "failed"       # broken
    unknown = "unknown"     # state indeterminate


# ── Measurement specs ─────────────────────────────────────────────────────────

@dataclass
class BandwidthSpec:
    capacity_bps: int
    available_bps: int
    used_bps: int
    quality: InfoQuality = InfoQuality.assumed

    def to_dict(self) -> dict:
        return {
            "capacity_bps": self.capacity_bps,
            "available_bps": self.available_bps,
            "used_bps": self.used_bps,
            "quality": self.quality.value,
        }


@dataclass
class LatencySpec:
    min_ms: float
    typical_ms: float
    max_ms: float
    quality: InfoQuality = InfoQuality.assumed

    def to_dict(self) -> dict:
        return {
            "min_ms": self.min_ms,
            "typical_ms": self.typical_ms,
            "max_ms": self.max_ms,
            "quality": self.quality.value,
        }


@dataclass
class JitterSpec:
    mean_ms: float
    p95_ms: float
    p99_ms: float
    quality: InfoQuality = InfoQuality.assumed

    def to_dict(self) -> dict:
        return {
            "mean_ms": self.mean_ms,
            "p95_ms": self.p95_ms,
            "p99_ms": self.p99_ms,
            "quality": self.quality.value,
        }


@dataclass
class LossSpec:
    rate: float           # 0.0–1.0
    window_seconds: int
    quality: InfoQuality = InfoQuality.assumed

    def to_dict(self) -> dict:
        return {
            "rate": self.rate,
            "window_seconds": self.window_seconds,
            "quality": self.quality.value,
        }


@dataclass
class ActivationTimeSpec:
    cold_to_warm_ms: float
    warm_to_active_ms: float
    active_to_warm_ms: float
    warm_to_standby_ms: float
    quality: InfoQuality = InfoQuality.assumed

    def to_dict(self) -> dict:
        return {
            "cold_to_warm_ms": self.cold_to_warm_ms,
            "warm_to_active_ms": self.warm_to_active_ms,
            "active_to_warm_ms": self.active_to_warm_ms,
            "warm_to_standby_ms": self.warm_to_standby_ms,
            "quality": self.quality.value,
        }


# ── Location ──────────────────────────────────────────────────────────────────

@dataclass
class PhysicalLocation:
    site: str | None = None
    space: str | None = None
    zone: str | None = None
    placement: str | None = None
    mobile: bool = False
    quality: InfoQuality = InfoQuality.assumed

    def to_dict(self) -> dict:
        d: dict[str, Any] = {"quality": self.quality.value, "mobile": self.mobile}
        for k in ("site", "space", "zone", "placement"):
            v = getattr(self, k)
            if v is not None:
                d[k] = v
        return d


@dataclass
class Location:
    machine_id: str | None = None
    bus: str | None = None
    bus_path: str | None = None
    overlay_ip: str | None = None
    physical: PhysicalLocation | None = None

    def to_dict(self) -> dict:
        d: dict[str, Any] = {}
        for k in ("machine_id", "bus", "bus_path", "overlay_ip"):
            v = getattr(self, k)
            if v is not None:
                d[k] = v
        if self.physical:
            d["physical"] = self.physical.to_dict()
        return d


# ── Hardware identity ─────────────────────────────────────────────────────────

@dataclass
class HardwareIdentity:
    serial_number: str | None = None
    serial_source: str | None = None
    uuid: str | None = None
    mac_addresses: list[str] = field(default_factory=list)
    usb_vid_pid: str | None = None
    usb_serial: str | None = None
    asset_tag: str | None = None

    def to_dict(self) -> dict:
        d: dict[str, Any] = {}
        for k in ("serial_number", "serial_source", "uuid", "usb_vid_pid",
                  "usb_serial", "asset_tag"):
            v = getattr(self, k)
            if v is not None:
                d[k] = v
        if self.mac_addresses:
            d["mac_addresses"] = self.mac_addresses
        return d


# ── Port ──────────────────────────────────────────────────────────────────────

@dataclass
class PortRef:
    device_id: str
    port_id: str

    def to_dict(self) -> dict:
        return {"device_id": self.device_id, "port_id": self.port_id}

    def __hash__(self) -> int:
        return hash((self.device_id, self.port_id))

    def __eq__(self, other: object) -> bool:
        if not isinstance(other, PortRef):
            return NotImplemented
        return self.device_id == other.device_id and self.port_id == other.port_id


@dataclass
class PortState:
    active: bool = False
    current_format: dict | None = None
    connected_to: list[PortRef] = field(default_factory=list)

    def to_dict(self) -> dict:
        d: dict[str, Any] = {"active": self.active}
        if self.current_format:
            d["current_format"] = self.current_format
        d["connected_to"] = [r.to_dict() for r in self.connected_to]
        return d


@dataclass
class Port:
    id: str
    device_id: str
    direction: PortDirection
    media_type: MediaType
    current_state: PortState = field(default_factory=PortState)
    properties: dict = field(default_factory=dict)
    label: str | None = None   # human-readable ("HDMI out", "USB gadget HID sink")
    # Declared capability set — what formats this port can produce or consume.
    # None means "unknown / accepts any format of this media type".
    # Populated by GraphBuilder from spec or node capability reports.
    format_set: "Any | None" = None   # FormatSet | None (imported lazily to avoid cycles)

    def to_dict(self) -> dict:
        d: dict[str, Any] = {
            "id": self.id,
            "device_id": self.device_id,
            "direction": self.direction.value,
            "media_type": self.media_type.value,
            "state": self.current_state.to_dict(),
        }
        if self.label:
            d["label"] = self.label
        if self.properties:
            d["properties"] = self.properties
        if self.format_set is not None:
            d["format_set"] = self.format_set.to_dict()
        return d


# ── Link ──────────────────────────────────────────────────────────────────────

@dataclass
class LinkState:
    status: LinkStatus = LinkStatus.unknown
    bandwidth: BandwidthSpec | None = None
    latency: LatencySpec | None = None
    jitter: JitterSpec | None = None
    loss: LossSpec | None = None
    activation_time: ActivationTimeSpec | None = None
    last_measured: float = field(default_factory=time.monotonic)

    def to_dict(self) -> dict:
        d: dict[str, Any] = {
            "status": self.status.value,
            "last_measured": self.last_measured,
        }
        for k in ("bandwidth", "latency", "jitter", "loss", "activation_time"):
            v = getattr(self, k)
            if v is not None:
                d[k] = v.to_dict()
        return d


@dataclass
class Link:
    id: str
    source: PortRef
    sink: PortRef
    transport: str        # transport plugin id, e.g. "udp_hid", "vban", "pipewire"
    state: LinkState = field(default_factory=LinkState)
    properties: dict = field(default_factory=dict)
    bidirectional: bool = False

    def to_dict(self) -> dict:
        d: dict[str, Any] = {
            "id": self.id,
            "source": self.source.to_dict(),
            "sink": self.sink.to_dict(),
            "transport": self.transport,
            "direction": "bidirectional" if self.bidirectional else "unidirectional",
            "state": self.state.to_dict(),
        }
        if self.properties:
            d["properties"] = self.properties
        return d


# ── Device ────────────────────────────────────────────────────────────────────

@dataclass
class Device:
    id: str
    name: str
    type: DeviceType
    location: Location = field(default_factory=Location)
    ports: list[Port] = field(default_factory=list)
    internal_links: list[Link] = field(default_factory=list)
    identity: HardwareIdentity | None = None
    properties: dict = field(default_factory=dict)

    def get_port(self, port_id: str) -> Port | None:
        for p in self.ports:
            if p.id == port_id:
                return p
        return None

    def ports_by_media(self, media_type: MediaType) -> list[Port]:
        return [p for p in self.ports if p.media_type == media_type]

    def to_dict(self) -> dict:
        d: dict[str, Any] = {
            "id": self.id,
            "name": self.name,
            "type": self.type.value,
            "location": self.location.to_dict(),
            "ports": [p.to_dict() for p in self.ports],
        }
        if self.internal_links:
            d["internal_links"] = [l.to_dict() for l in self.internal_links]
        if self.identity:
            d["identity"] = self.identity.to_dict()
        if self.properties:
            d["properties"] = self.properties
        return d
