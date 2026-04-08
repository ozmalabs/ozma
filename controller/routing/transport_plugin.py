# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Transport plugin system for the Ozma routing graph — Phase 4.

Implements the TransportPlugin interface and built-in transport
characteristic table from docs/routing/transports.md.

A transport plugin describes a physical or logical mechanism for
carrying a media stream between two ports. Plugins are responsible for:
  - Discovering links that exist via this transport
  - Reporting the format capabilities of those links
  - Measuring link metrics (latency, jitter, loss, bandwidth)
  - Opening/closing active streams

Phase 4 adds the plugin interface and the default characteristic table.
Actual transport implementations (UDP, WireGuard, etc.) are out of scope
here — they live in the relevant protocol modules.
"""

from __future__ import annotations

from abc import ABC, abstractmethod
from dataclasses import dataclass, field
from enum import Enum
from typing import Any

from .model import (
    ActivationTimeSpec,
    BandwidthSpec,
    InfoQuality,
    JitterSpec,
    LatencySpec,
    Link,
    LossSpec,
    MediaType,
)
from .formats import FormatSet
from .pipeline import LinkRef


# ── Channel priority ──────────────────────────────────────────────────────────

class ChannelPriority(str, Enum):
    realtime = "realtime"  # HID, control — never delayed
    high = "high"          # audio — low latency, moderate BW
    normal = "normal"      # video — high BW, can tolerate brief delays
    low = "low"            # sensors, RGB, screen — best effort
    bulk = "bulk"          # file transfer, firmware — remaining BW


# ── Flow control ──────────────────────────────────────────────────────────────

@dataclass
class FlowControl:
    window_bytes: int | None = None
    max_packet_size: int | None = None
    rate_limit_bps: int | None = None

    def to_dict(self) -> dict:
        return {
            "window_bytes": self.window_bytes,
            "max_packet_size": self.max_packet_size,
            "rate_limit_bps": self.rate_limit_bps,
        }


# ── Channel ───────────────────────────────────────────────────────────────────

@dataclass
class ChannelStats:
    bytes_sent: int = 0
    bytes_received: int = 0
    packets_sent: int = 0
    packets_lost: int = 0
    latency_ms: float = 0.0

    def to_dict(self) -> dict:
        return {
            "bytes_sent": self.bytes_sent,
            "bytes_received": self.bytes_received,
            "packets_sent": self.packets_sent,
            "packets_lost": self.packets_lost,
            "latency_ms": self.latency_ms,
        }


@dataclass
class Channel:
    id: int
    name: str                                    # "hid", "audio-vban", "video-h264"
    type: str = "data"
    priority: ChannelPriority = ChannelPriority.normal
    flow_control: FlowControl = field(default_factory=FlowControl)
    state: str = "open"                          # "open", "half_closed", "closed"
    stats: ChannelStats = field(default_factory=ChannelStats)

    def to_dict(self) -> dict:
        return {
            "id": self.id,
            "name": self.name,
            "type": self.type,
            "priority": self.priority.value,
            "state": self.state,
            "stats": self.stats.to_dict(),
        }


# ── Rekey policy ──────────────────────────────────────────────────────────────

@dataclass
class RekeyPolicy:
    max_bytes: int = 1_000_000_000   # 1 GB
    max_seconds: int = 3600          # 1 hour
    algorithm: str = "noise_xx"      # "noise_xx", "noise_nk"

    def to_dict(self) -> dict:
        return {
            "max_bytes": self.max_bytes,
            "max_seconds": self.max_seconds,
            "algorithm": self.algorithm,
        }


# ── Multiplexed connection ────────────────────────────────────────────────────

class ConnectionState(str, Enum):
    establishing = "establishing"
    active = "active"
    rekeying = "rekeying"
    draining = "draining"
    closed = "closed"


@dataclass
class MultiplexedConnection:
    """
    A single multiplexed connection between two devices, potentially
    shared by multiple Pipelines via different Channels.
    """
    id: str
    transport: str
    local_device_id: str
    remote_device_id: str
    state: ConnectionState = ConnectionState.establishing
    channels: list[Channel] = field(default_factory=list)
    rekey_policy: RekeyPolicy = field(default_factory=RekeyPolicy)
    shared_by_pipelines: list[str] = field(default_factory=list)

    def add_channel(self, channel: Channel) -> None:
        self.channels.append(channel)

    def channel_for(self, name: str) -> Channel | None:
        for ch in self.channels:
            if ch.name == name:
                return ch
        return None

    def to_dict(self) -> dict:
        return {
            "id": self.id,
            "transport": self.transport,
            "local_device_id": self.local_device_id,
            "remote_device_id": self.remote_device_id,
            "state": self.state.value,
            "channels": [ch.to_dict() for ch in self.channels],
            "shared_by_pipelines": list(self.shared_by_pipelines),
        }


# ── Link metrics snapshot ─────────────────────────────────────────────────────

@dataclass
class LinkMetrics:
    """Live measurements returned by TransportPlugin.measure()."""
    bandwidth: BandwidthSpec | None = None
    latency: LatencySpec | None = None
    jitter: JitterSpec | None = None
    loss: LossSpec | None = None
    quality: InfoQuality = InfoQuality.measured

    def to_dict(self) -> dict:
        return {
            "bandwidth": self.bandwidth.to_dict() if self.bandwidth else None,
            "latency": self.latency.to_dict() if self.latency else None,
            "jitter": self.jitter.to_dict() if self.jitter else None,
            "loss": self.loss.to_dict() if self.loss else None,
            "quality": self.quality.value,
        }


# ── Stream handle ─────────────────────────────────────────────────────────────

@dataclass
class StreamHandle:
    """Opaque handle representing an open stream on a transport."""
    id: str
    link_ref: LinkRef
    transport_id: str
    channel: Channel | None = None

    def to_dict(self) -> dict:
        return {
            "id": self.id,
            "link_ref": self.link_ref.to_dict(),
            "transport_id": self.transport_id,
            "channel": self.channel.to_dict() if self.channel else None,
        }


# ── Transport characteristics ─────────────────────────────────────────────────

@dataclass
class TransportCharacteristics:
    """
    Baseline expected quality for a transport type.

    These are `spec` quality values — they are used before active
    measurement provides `measured` data (Phase 5).
    """
    latency: LatencySpec = field(default_factory=lambda: LatencySpec(0, 1, 5, InfoQuality.spec))
    jitter: JitterSpec = field(default_factory=lambda: JitterSpec(0, 1, 5, InfoQuality.spec))
    loss: LossSpec = field(default_factory=lambda: LossSpec(0.0, 60, InfoQuality.spec))
    bandwidth: BandwidthSpec | None = None
    requires_network: bool = True
    supports_multicast: bool = False
    supports_encryption: bool = False
    encryption_overhead_bps: int = 0

    def to_dict(self) -> dict:
        return {
            "latency": self.latency.to_dict(),
            "jitter": self.jitter.to_dict(),
            "loss": self.loss.to_dict(),
            "bandwidth": self.bandwidth.to_dict() if self.bandwidth else None,
            "requires_network": self.requires_network,
            "supports_multicast": self.supports_multicast,
            "supports_encryption": self.supports_encryption,
            "encryption_overhead_bps": self.encryption_overhead_bps,
        }


def _tc(
    latency_p50_ms: float,
    jitter_p95_ms: float,
    loss_rate: float,
    bandwidth_gbps: float | None = None,
    requires_network: bool = True,
    supports_multicast: bool = False,
    supports_encryption: bool = False,
    encryption_overhead_bps: int = 0,
) -> TransportCharacteristics:
    """Helper to build a TransportCharacteristics from spec table values."""
    bw = BandwidthSpec(
        capacity_bps=int(bandwidth_gbps * 1e9),
        available_bps=int(bandwidth_gbps * 1e9),
        used_bps=0,
        quality=InfoQuality.spec,
    ) if bandwidth_gbps else None
    return TransportCharacteristics(
        latency=LatencySpec(
            min_ms=latency_p50_ms * 0.5,
            typical_ms=latency_p50_ms,
            max_ms=latency_p50_ms * 4,
            quality=InfoQuality.spec,
        ),
        jitter=JitterSpec(
            mean_ms=jitter_p95_ms * 0.5,
            p95_ms=jitter_p95_ms,
            p99_ms=jitter_p95_ms * 2,
            quality=InfoQuality.spec,
        ),
        loss=LossSpec(rate=loss_rate, window_seconds=60, quality=InfoQuality.spec),
        bandwidth=bw,
        requires_network=requires_network,
        supports_multicast=supports_multicast,
        supports_encryption=supports_encryption,
        encryption_overhead_bps=encryption_overhead_bps,
    )


# ── Built-in transport characteristic table ───────────────────────────────────
# Values from docs/routing/transports.md §Expected Link Characteristics

TRANSPORT_CHARACTERISTICS: dict[str, TransportCharacteristics] = {
    "local-pipe": _tc(
        latency_p50_ms=0.05, jitter_p95_ms=0.01, loss_rate=0.0,
        requires_network=False, bandwidth_gbps=100.0,
    ),
    "pipewire": _tc(
        latency_p50_ms=0.5, jitter_p95_ms=0.1, loss_rate=0.0,
        requires_network=False, bandwidth_gbps=10.0,
    ),
    "usb-gadget": _tc(
        latency_p50_ms=0.5, jitter_p95_ms=0.5, loss_rate=0.0,
        requires_network=False, bandwidth_gbps=0.48,
    ),
    "v4l2": _tc(
        latency_p50_ms=2.0, jitter_p95_ms=1.0, loss_rate=0.0,
        requires_network=False,
    ),
    "udp-direct": _tc(
        latency_p50_ms=0.3, jitter_p95_ms=0.1, loss_rate=0.00001,
        requires_network=True, supports_multicast=True,
    ),
    "udp-aead": _tc(
        latency_p50_ms=0.3, jitter_p95_ms=0.1, loss_rate=0.00001,
        requires_network=True, supports_encryption=True,
        encryption_overhead_bps=200_000,
    ),
    "udp-direct-wifi": _tc(
        latency_p50_ms=2.5, jitter_p95_ms=5.0, loss_rate=0.005,
        requires_network=True,
    ),
    "wireguard": _tc(
        latency_p50_ms=0.7, jitter_p95_ms=0.2, loss_rate=0.00001,
        requires_network=True, supports_encryption=True,
        encryption_overhead_bps=300_000,
    ),
    "wireguard-internet-fibre": _tc(
        latency_p50_ms=15.0, jitter_p95_ms=3.0, loss_rate=0.001,
        requires_network=True, supports_encryption=True,
    ),
    "wireguard-internet-cable": _tc(
        latency_p50_ms=30.0, jitter_p95_ms=8.0, loss_rate=0.003,
        requires_network=True, supports_encryption=True,
    ),
    "wireguard-internet-lte": _tc(
        latency_p50_ms=40.0, jitter_p95_ms=25.0, loss_rate=0.02,
        requires_network=True, supports_encryption=True,
    ),
    "wireguard-satellite-leo": _tc(
        latency_p50_ms=40.0, jitter_p95_ms=15.0, loss_rate=0.015,
        requires_network=True, supports_encryption=True,
    ),
    "wireguard-satellite-geo": _tc(
        latency_p50_ms=600.0, jitter_p95_ms=30.0, loss_rate=0.03,
        requires_network=True, supports_encryption=True,
    ),
    "bluetooth-a2dp": _tc(
        latency_p50_ms=40.0, jitter_p95_ms=10.0, loss_rate=0.005,
        requires_network=False, bandwidth_gbps=0.000990,
    ),
    "bluetooth-hid": _tc(
        latency_p50_ms=10.0, jitter_p95_ms=5.0, loss_rate=0.002,
        requires_network=False,
    ),
    "bluetooth-le-audio": _tc(
        latency_p50_ms=8.0, jitter_p95_ms=3.0, loss_rate=0.002,
        requires_network=False,
    ),
    "serial": _tc(
        latency_p50_ms=0.5, jitter_p95_ms=0.5, loss_rate=0.0,
        requires_network=False,
    ),
    "websocket-lan": _tc(
        latency_p50_ms=2.0, jitter_p95_ms=1.0, loss_rate=0.0,
        requires_network=True,
    ),
    "websocket-internet": _tc(
        latency_p50_ms=40.0, jitter_p95_ms=15.0, loss_rate=0.002,
        requires_network=True, supports_encryption=True,
    ),
    "webrtc-lan": _tc(
        latency_p50_ms=2.0, jitter_p95_ms=1.0, loss_rate=0.001,
        requires_network=True, supports_encryption=True,
    ),
    "webrtc-internet": _tc(
        latency_p50_ms=40.0, jitter_p95_ms=15.0, loss_rate=0.01,
        requires_network=True, supports_encryption=True,
    ),
    "sunshine": _tc(
        latency_p50_ms=8.0, jitter_p95_ms=2.5, loss_rate=0.001,
        requires_network=True,
    ),
    "vban": _tc(
        latency_p50_ms=1.0, jitter_p95_ms=0.5, loss_rate=0.0,
        requires_network=True, supports_multicast=True,
    ),
    "cec": _tc(
        latency_p50_ms=100.0, jitter_p95_ms=50.0, loss_rate=0.005,
        requires_network=False,
    ),
    "ddc-ci": _tc(
        latency_p50_ms=50.0, jitter_p95_ms=20.0, loss_rate=0.0,
        requires_network=False,
    ),
    "ir": _tc(
        latency_p50_ms=100.0, jitter_p95_ms=30.0, loss_rate=0.02,
        requires_network=False,
    ),
    "mqtt": _tc(
        latency_p50_ms=20.0, jitter_p95_ms=10.0, loss_rate=0.002,
        requires_network=True,
    ),
    "qmp": _tc(
        latency_p50_ms=1.0, jitter_p95_ms=0.5, loss_rate=0.0,
        requires_network=False,
    ),
}


def get_transport_characteristics(transport_id: str) -> TransportCharacteristics:
    """
    Return baseline characteristics for a transport ID.

    Falls back to a conservative default (LAN UDP) if unknown.
    """
    return TRANSPORT_CHARACTERISTICS.get(
        transport_id,
        _tc(latency_p50_ms=5.0, jitter_p95_ms=5.0, loss_rate=0.005),
    )


# ── TransportPlugin abstract interface ────────────────────────────────────────

class TransportPlugin(ABC):
    """
    Abstract base class for Ozma transport plugins.

    A transport plugin encapsulates one physical or logical transport
    mechanism. Concrete subclasses are registered with the TransportRegistry
    and used by the Router to discover links and open streams.
    """

    @property
    @abstractmethod
    def id(self) -> str:
        """Unique identifier, e.g. 'udp-aead', 'wireguard', 'pipewire'."""
        ...

    @property
    @abstractmethod
    def name(self) -> str:
        """Human-readable display name."""
        ...

    @property
    def supported_media_types(self) -> list[MediaType]:
        """Media types this transport can carry. Default: all."""
        return list(MediaType)

    @property
    def characteristics(self) -> TransportCharacteristics:
        """Baseline expected quality before active measurement."""
        return get_transport_characteristics(self.id)

    @abstractmethod
    def discover_links(self) -> list[Link]:
        """
        Enumerate all links discoverable via this transport.

        Called when the graph is rebuilt (on startup and on topology change).
        """
        ...

    @abstractmethod
    def capabilities(self, link_ref: LinkRef) -> FormatSet:
        """Return the format capabilities for a specific link."""
        ...

    def measure(self, link_ref: LinkRef) -> LinkMetrics:
        """
        Measure current link metrics.

        Default implementation returns spec-quality characteristics.
        Override in Phase 5 (active measurement).
        """
        tc = self.characteristics
        return LinkMetrics(
            bandwidth=tc.bandwidth,
            latency=tc.latency,
            jitter=tc.jitter,
            loss=tc.loss,
            quality=InfoQuality.spec,
        )

    def open(self, link_ref: LinkRef, channel: Channel) -> StreamHandle:
        """
        Open an active stream on this transport.

        Returns a StreamHandle that must be passed to close().
        Default is a no-op (in-memory stub).
        """
        import uuid
        return StreamHandle(
            id=str(uuid.uuid4()),
            link_ref=link_ref,
            transport_id=self.id,
            channel=channel,
        )

    def close(self, stream: StreamHandle) -> None:
        """Close a previously opened stream."""
        pass

    def on_link_change(self, callback) -> None:
        """
        Register a callback to be invoked when link topology changes.

        callback signature: (added: list[Link], removed: list[str]) -> None
        """
        pass


# ── Transport registry ────────────────────────────────────────────────────────

class TransportRegistry:
    """
    Central registry of available TransportPlugins.

    The Router consults this to enumerate links and measure metrics.
    """

    def __init__(self) -> None:
        self._plugins: dict[str, TransportPlugin] = {}

    def register(self, plugin: TransportPlugin) -> None:
        self._plugins[plugin.id] = plugin

    def unregister(self, transport_id: str) -> bool:
        return self._plugins.pop(transport_id, None) is not None

    def get(self, transport_id: str) -> TransportPlugin | None:
        return self._plugins.get(transport_id)

    def list_all(self) -> list[TransportPlugin]:
        return list(self._plugins.values())

    def plugins_for_media_type(self, mt: MediaType) -> list[TransportPlugin]:
        return [p for p in self._plugins.values() if mt in p.supported_media_types]

    def discover_all_links(self) -> list[Link]:
        links: list[Link] = []
        for plugin in self._plugins.values():
            try:
                links.extend(plugin.discover_links())
            except Exception:
                pass
        return links


# ── Bluetooth link state ──────────────────────────────────────────────────────

class BluetoothProfile(str, Enum):
    a2dp_source = "a2dp_source"
    a2dp_sink = "a2dp_sink"
    hfp = "hfp"
    hfp_wideband = "hfp_wideband"
    hid = "hid"
    le_audio_unicast = "le_audio_unicast"
    le_audio_broadcast = "le_audio_broadcast"
    ble_gatt = "ble_gatt"
    spp = "spp"
    pan = "pan"


@dataclass
class BluetoothCodecInfo:
    name: str                          # "sbc", "aac", "aptx", "aptx_hd", "ldac", "lc3"
    bitrate_kbps: int | None = None
    sample_rate_hz: int | None = None
    bit_depth: int | None = None
    channels: int | None = None
    latency_ms: float | None = None
    lossy: bool = True
    quality: InfoQuality = InfoQuality.reported

    # Latency reference data from spec table
    CODEC_LATENCY_MS: dict[str, float] = field(default_factory=lambda: {
        "sbc": 40.0, "aac": 60.0, "aptx": 40.0, "aptx_hd": 40.0,
        "aptx_adaptive": 60.0, "aptx_lossless": 40.0,
        "ldac": 50.0, "lc3": 8.0, "lc3plus": 7.0,
        "msbc": 10.0, "cvsd": 10.0,
    })

    def to_dict(self) -> dict:
        return {
            "name": self.name,
            "bitrate_kbps": self.bitrate_kbps,
            "sample_rate_hz": self.sample_rate_hz,
            "bit_depth": self.bit_depth,
            "channels": self.channels,
            "latency_ms": self.latency_ms,
            "lossy": self.lossy,
        }


@dataclass
class BluetoothConnection:
    rssi_dbm: int | None = None
    tx_power_dbm: int | None = None
    link_quality: int | None = None     # 0–255
    distance_estimate_m: float | None = None
    version: str | None = None          # "5.2"
    phy: str | None = None              # "1m", "2m", "coded"
    mtu: int | None = None
    connection_interval_ms: float | None = None

    def to_dict(self) -> dict:
        return {k: v for k, v in self.__dict__.items() if v is not None}


@dataclass
class BluetoothLinkState:
    profile: BluetoothProfile = BluetoothProfile.a2dp_source
    codec: BluetoothCodecInfo | None = None
    connection: BluetoothConnection = field(default_factory=BluetoothConnection)
    device_name: str = ""
    device_address: str = ""
    paired: bool = False
    bonded: bool = False
    battery_percent: int | None = None

    def to_dict(self) -> dict:
        return {
            "profile": self.profile.value,
            "codec": self.codec.to_dict() if self.codec else None,
            "connection": self.connection.to_dict(),
            "device_name": self.device_name,
            "device_address": self.device_address,
            "paired": self.paired,
            "bonded": self.bonded,
            "battery_percent": self.battery_percent,
        }


# ── WiFi link state ───────────────────────────────────────────────────────────

@dataclass
class WiFiSignalQuality:
    rssi_dbm: int = -70
    noise_dbm: int | None = None
    snr_db: float | None = None
    quality_percent: float | None = None
    quality: InfoQuality = InfoQuality.measured

    @property
    def signal_level(self) -> str:
        """Qualitative signal level from RSSI."""
        if self.rssi_dbm > -50:
            return "excellent"
        if self.rssi_dbm > -60:
            return "good"
        if self.rssi_dbm > -70:
            return "fair"
        if self.rssi_dbm > -80:
            return "poor"
        return "unusable"

    def to_dict(self) -> dict:
        d: dict[str, Any] = {
            "rssi_dbm": self.rssi_dbm,
            "quality": self.quality.value,
            "signal_level": self.signal_level,
        }
        for attr in ("noise_dbm", "snr_db", "quality_percent"):
            v = getattr(self, attr)
            if v is not None:
                d[attr] = v
        return d


@dataclass
class WiFiLinkRate:
    tx_rate_mbps: float = 0.0
    rx_rate_mbps: float = 0.0
    mcs_index: int | None = None
    spatial_streams: int | None = None
    estimated_throughput_mbps: float | None = None

    def to_dict(self) -> dict:
        d: dict[str, Any] = {
            "tx_rate_mbps": self.tx_rate_mbps,
            "rx_rate_mbps": self.rx_rate_mbps,
        }
        for attr in ("mcs_index", "spatial_streams", "estimated_throughput_mbps"):
            v = getattr(self, attr)
            if v is not None:
                d[attr] = v
        return d


@dataclass
class WiFiLinkState:
    interface: str = "wlan0"
    standard: str = "wifi5"     # "wifi4", "wifi5", "wifi6", "wifi6e", "wifi7"
    band: str = "5ghz"          # "2.4ghz", "5ghz", "6ghz"
    channel: int = 36
    channel_width_mhz: int = 80
    signal: WiFiSignalQuality = field(default_factory=WiFiSignalQuality)
    link_rate: WiFiLinkRate = field(default_factory=WiFiLinkRate)
    ssid: str = ""
    bssid: str = ""
    channel_utilisation_percent: float | None = None

    def effective_degradation_factor(self) -> float:
        """
        Returns a multiplier (0.0–1.0) representing how much this WiFi
        link degrades expected performance.

        Per spec: if channel_utilisation > 70%, treat as if RSSI were 10 dBm worse.
        """
        rssi = self.signal.rssi_dbm
        if self.channel_utilisation_percent and self.channel_utilisation_percent > 70:
            rssi -= 10
        if rssi > -50:
            return 1.0
        if rssi > -60:
            return 0.8
        if rssi > -70:
            return 0.6
        if rssi > -80:
            return 0.3
        return 0.1

    def to_dict(self) -> dict:
        return {
            "interface": self.interface,
            "standard": self.standard,
            "band": self.band,
            "channel": self.channel,
            "channel_width_mhz": self.channel_width_mhz,
            "signal": self.signal.to_dict(),
            "link_rate": self.link_rate.to_dict(),
            "ssid": self.ssid,
            "bssid": self.bssid,
            "channel_utilisation_percent": self.channel_utilisation_percent,
        }


# ── Serial link state ─────────────────────────────────────────────────────────

class SerialProtocol(str, Enum):
    raw = "raw"
    modbus_rtu = "modbus_rtu"
    modbus_ascii = "modbus_ascii"
    dmx512 = "dmx512"
    midi_din = "midi_din"
    nmea = "nmea"
    at_commands = "at_commands"
    custom = "custom"


@dataclass
class SerialLinkState:
    port: str = "/dev/ttyUSB0"
    interface_type: str = "usb_serial"    # "rs232","rs485","uart","usb_serial","virtual"
    baud_rate: int = 115200
    data_bits: int = 8
    parity: str = "none"                  # "none","even","odd","mark","space"
    stop_bits: float = 1.0
    flow_control: str = "none"            # "none","rts_cts","xon_xoff"
    protocol: SerialProtocol = SerialProtocol.raw
    usb_path: str | None = None
    usb_chipset: str | None = None
    persistent_id: str | None = None

    @property
    def throughput_bytes_per_sec(self) -> int:
        """Effective throughput using spec formula."""
        from .formats import serial_bandwidth_bytes_per_sec
        return serial_bandwidth_bytes_per_sec(
            self.baud_rate, self.data_bits, self.parity, self.stop_bits
        )

    def to_dict(self) -> dict:
        d: dict[str, Any] = {
            "port": self.port,
            "interface_type": self.interface_type,
            "baud_rate": self.baud_rate,
            "data_bits": self.data_bits,
            "parity": self.parity,
            "stop_bits": self.stop_bits,
            "flow_control": self.flow_control,
            "protocol": self.protocol.value,
            "throughput_bytes_per_sec": self.throughput_bytes_per_sec,
        }
        for attr in ("usb_path", "usb_chipset", "persistent_id"):
            v = getattr(self, attr)
            if v is not None:
                d[attr] = v
        return d
