# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for the transport plugin system (Phase 4)."""

from __future__ import annotations

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent.parent.parent / "controller"))

import pytest
from routing.model import (
    InfoQuality,
    Link,
    MediaType,
    PortRef,
)
from routing.formats import FormatSet, FormatRange
from routing.pipeline import LinkRef
from routing.transport_plugin import (
    BluetoothCodecInfo,
    BluetoothConnection,
    BluetoothLinkState,
    BluetoothProfile,
    Channel,
    ChannelPriority,
    ChannelStats,
    ConnectionState,
    FlowControl,
    LinkMetrics,
    MultiplexedConnection,
    RekeyPolicy,
    SerialLinkState,
    SerialProtocol,
    StreamHandle,
    TRANSPORT_CHARACTERISTICS,
    TransportCharacteristics,
    TransportPlugin,
    TransportRegistry,
    WiFiLinkRate,
    WiFiLinkState,
    WiFiSignalQuality,
    get_transport_characteristics,
)

pytestmark = pytest.mark.unit


# ── ChannelPriority ───────────────────────────────────────────────────────────

class TestChannelPriority:
    def test_values(self):
        assert ChannelPriority.realtime.value == "realtime"
        assert ChannelPriority.bulk.value == "bulk"

    def test_all_five_defined(self):
        assert len(ChannelPriority) == 5


# ── Channel ───────────────────────────────────────────────────────────────────

class TestChannel:
    def test_defaults(self):
        ch = Channel(id=1, name="hid")
        assert ch.priority == ChannelPriority.normal
        assert ch.state == "open"

    def test_to_dict(self):
        ch = Channel(id=0, name="hid", priority=ChannelPriority.realtime)
        d = ch.to_dict()
        assert d["name"] == "hid"
        assert d["priority"] == "realtime"
        assert "stats" in d


# ── RekeyPolicy ───────────────────────────────────────────────────────────────

class TestRekeyPolicy:
    def test_defaults(self):
        r = RekeyPolicy()
        assert r.max_bytes == 1_000_000_000
        assert r.max_seconds == 3600
        assert r.algorithm == "noise_xx"

    def test_to_dict(self):
        r = RekeyPolicy(max_bytes=500_000_000, algorithm="noise_nk")
        d = r.to_dict()
        assert d["max_bytes"] == 500_000_000
        assert d["algorithm"] == "noise_nk"


# ── MultiplexedConnection ─────────────────────────────────────────────────────

class TestMultiplexedConnection:
    def _make(self):
        return MultiplexedConnection(
            id="conn-1",
            transport="udp-aead",
            local_device_id="ctrl",
            remote_device_id="node-a",
        )

    def test_defaults(self):
        c = self._make()
        assert c.state == ConnectionState.establishing
        assert c.channels == []

    def test_add_channel(self):
        c = self._make()
        ch = Channel(id=1, name="hid")
        c.add_channel(ch)
        assert len(c.channels) == 1

    def test_channel_for(self):
        c = self._make()
        c.add_channel(Channel(id=1, name="hid"))
        c.add_channel(Channel(id=2, name="audio"))
        assert c.channel_for("hid").name == "hid"
        assert c.channel_for("audio").name == "audio"
        assert c.channel_for("video") is None

    def test_to_dict(self):
        c = self._make()
        d = c.to_dict()
        assert d["transport"] == "udp-aead"
        assert d["local_device_id"] == "ctrl"
        assert isinstance(d["channels"], list)


# ── TransportCharacteristics ──────────────────────────────────────────────────

class TestTransportCharacteristics:
    def test_local_pipe_low_latency(self):
        tc = TRANSPORT_CHARACTERISTICS["local-pipe"]
        assert tc.latency.typical_ms < 1.0
        assert tc.loss.rate == 0.0
        assert not tc.requires_network

    def test_wireguard_encrypted(self):
        tc = TRANSPORT_CHARACTERISTICS["wireguard"]
        assert tc.supports_encryption
        assert tc.encryption_overhead_bps > 0

    def test_udp_direct_multicast(self):
        tc = TRANSPORT_CHARACTERISTICS["udp-direct"]
        assert tc.supports_multicast

    def test_bluetooth_a2dp_no_network(self):
        tc = TRANSPORT_CHARACTERISTICS["bluetooth-a2dp"]
        assert not tc.requires_network

    def test_cec_high_latency(self):
        tc = TRANSPORT_CHARACTERISTICS["cec"]
        assert tc.latency.typical_ms >= 50.0

    def test_satellite_geo_very_high_latency(self):
        tc = TRANSPORT_CHARACTERISTICS["wireguard-satellite-geo"]
        assert tc.latency.typical_ms >= 500.0

    def test_pipewire_no_network(self):
        tc = TRANSPORT_CHARACTERISTICS["pipewire"]
        assert not tc.requires_network
        assert tc.loss.rate == 0.0

    def test_to_dict(self):
        tc = TRANSPORT_CHARACTERISTICS["udp-aead"]
        d = tc.to_dict()
        assert "latency" in d
        assert "jitter" in d
        assert "supports_encryption" in d


class TestGetTransportCharacteristics:
    def test_known_transport(self):
        tc = get_transport_characteristics("wireguard")
        assert tc.supports_encryption

    def test_unknown_transport_returns_default(self):
        tc = get_transport_characteristics("some-unknown-plugin")
        # Should return a valid default, not raise
        assert tc is not None
        assert tc.latency.typical_ms > 0

    def test_all_builtin_transports_have_characteristics(self):
        expected = [
            "local-pipe", "pipewire", "usb-gadget", "v4l2",
            "udp-direct", "udp-aead", "wireguard", "bluetooth-a2dp",
            "serial", "cec", "ir", "vban",
        ]
        for t in expected:
            tc = get_transport_characteristics(t)
            assert tc is not None, f"Missing characteristics for {t}"


# ── TransportPlugin abstract interface ────────────────────────────────────────

class TestTransportPlugin:
    def _make_plugin(self):
        """Create a minimal stub TransportPlugin."""

        class StubPlugin(TransportPlugin):
            @property
            def id(self):
                return "stub-test"

            @property
            def name(self):
                return "Stub transport"

            def discover_links(self):
                return []

            def capabilities(self, link_ref):
                return FormatSet(formats=[
                    FormatRange(media_type=MediaType.hid)
                ])

        return StubPlugin()

    def test_basic_interface(self):
        p = self._make_plugin()
        assert p.id == "stub-test"
        assert p.name == "Stub transport"

    def test_supported_media_types_default(self):
        p = self._make_plugin()
        # Default returns all types
        assert len(p.supported_media_types) > 0

    def test_discover_links_empty(self):
        p = self._make_plugin()
        assert p.discover_links() == []

    def test_capabilities_returns_formatset(self):
        p = self._make_plugin()
        lr = LinkRef("link-1")
        fs = p.capabilities(lr)
        assert isinstance(fs, FormatSet)

    def test_measure_returns_spec_quality(self):
        p = self._make_plugin()
        lr = LinkRef("link-1")
        m = p.measure(lr)
        assert m.quality == InfoQuality.spec

    def test_open_returns_handle(self):
        p = self._make_plugin()
        lr = LinkRef("link-1")
        ch = Channel(id=0, name="hid")
        handle = p.open(lr, ch)
        assert isinstance(handle, StreamHandle)
        assert handle.transport_id == "stub-test"

    def test_close_is_noop(self):
        p = self._make_plugin()
        lr = LinkRef("link-1")
        ch = Channel(id=0, name="hid")
        handle = p.open(lr, ch)
        p.close(handle)  # should not raise


# ── TransportRegistry ─────────────────────────────────────────────────────────

class TestTransportRegistry:
    def _plugin(self, pid):
        class P(TransportPlugin):
            @property
            def id(self): return pid
            @property
            def name(self): return pid
            def discover_links(self): return []
            def capabilities(self, lr): return FormatSet()
        return P()

    def test_register_and_get(self):
        reg = TransportRegistry()
        p = self._plugin("udp-aead")
        reg.register(p)
        assert reg.get("udp-aead") is p

    def test_unregister(self):
        reg = TransportRegistry()
        reg.register(self._plugin("udp-aead"))
        assert reg.unregister("udp-aead")
        assert reg.get("udp-aead") is None

    def test_unregister_missing_returns_false(self):
        reg = TransportRegistry()
        assert not reg.unregister("nope")

    def test_list_all(self):
        reg = TransportRegistry()
        reg.register(self._plugin("a"))
        reg.register(self._plugin("b"))
        assert len(reg.list_all()) == 2

    def test_plugins_for_media_type(self):
        class HidOnlyPlugin(TransportPlugin):
            @property
            def id(self): return "hid-only"
            @property
            def name(self): return "HID only"
            @property
            def supported_media_types(self): return [MediaType.hid]
            def discover_links(self): return []
            def capabilities(self, lr): return FormatSet()

        reg = TransportRegistry()
        reg.register(HidOnlyPlugin())
        reg.register(self._plugin("all-types"))
        hid_plugins = reg.plugins_for_media_type(MediaType.hid)
        assert len(hid_plugins) == 2  # both support hid
        vid_plugins = reg.plugins_for_media_type(MediaType.video)
        # HidOnlyPlugin doesn't support video
        assert len(vid_plugins) == 1

    def test_discover_all_links_aggregates(self):
        reg = TransportRegistry()
        reg.register(self._plugin("t1"))
        reg.register(self._plugin("t2"))
        # Both return empty lists by default
        assert reg.discover_all_links() == []


# ── BluetoothLinkState ────────────────────────────────────────────────────────

class TestBluetoothLinkState:
    def test_defaults(self):
        b = BluetoothLinkState()
        assert b.profile == BluetoothProfile.a2dp_source
        assert not b.paired

    def test_codec_info(self):
        codec = BluetoothCodecInfo(name="ldac", bitrate_kbps=990, lossy=True)
        b = BluetoothLinkState(codec=codec)
        assert b.codec.name == "ldac"

    def test_to_dict(self):
        b = BluetoothLinkState(
            profile=BluetoothProfile.hid,
            device_address="AA:BB:CC:DD:EE:FF",
            paired=True,
        )
        d = b.to_dict()
        assert d["profile"] == "hid"
        assert d["paired"] is True
        assert d["device_address"] == "AA:BB:CC:DD:EE:FF"

    def test_all_profiles_defined(self):
        assert len(BluetoothProfile) == 10


# ── WiFiLinkState ─────────────────────────────────────────────────────────────

class TestWiFiLinkState:
    def test_signal_levels(self):
        assert WiFiSignalQuality(rssi_dbm=-40).signal_level == "excellent"
        assert WiFiSignalQuality(rssi_dbm=-55).signal_level == "good"
        assert WiFiSignalQuality(rssi_dbm=-65).signal_level == "fair"
        assert WiFiSignalQuality(rssi_dbm=-75).signal_level == "poor"
        assert WiFiSignalQuality(rssi_dbm=-85).signal_level == "unusable"

    def test_degradation_excellent_signal(self):
        w = WiFiLinkState(signal=WiFiSignalQuality(rssi_dbm=-40))
        assert w.effective_degradation_factor() == 1.0

    def test_degradation_poor_signal(self):
        w = WiFiLinkState(signal=WiFiSignalQuality(rssi_dbm=-75))
        assert w.effective_degradation_factor() < 0.5

    def test_degradation_high_channel_utilisation(self):
        # -55 dBm (good) but 80% channel utilisation → treat as -65 dBm (fair)
        w = WiFiLinkState(
            signal=WiFiSignalQuality(rssi_dbm=-55),
            channel_utilisation_percent=80.0,
        )
        # -55 - 10 = -65 → fair → 0.6
        assert w.effective_degradation_factor() == pytest.approx(0.6)

    def test_to_dict(self):
        w = WiFiLinkState(ssid="HomeNetwork", bssid="AA:BB:CC:DD:EE:FF")
        d = w.to_dict()
        assert d["ssid"] == "HomeNetwork"
        assert "signal" in d
        assert "link_rate" in d


# ── SerialLinkState ───────────────────────────────────────────────────────────

class TestSerialLinkState:
    def test_throughput_115200_8n1(self):
        s = SerialLinkState(baud_rate=115200, data_bits=8, parity="none", stop_bits=1.0)
        assert s.throughput_bytes_per_sec == 11520

    def test_throughput_9600(self):
        s = SerialLinkState(baud_rate=9600)
        assert s.throughput_bytes_per_sec == 960

    def test_default_protocol(self):
        s = SerialLinkState()
        assert s.protocol == SerialProtocol.raw

    def test_to_dict(self):
        s = SerialLinkState(port="/dev/ttyUSB0", baud_rate=9600)
        d = s.to_dict()
        assert d["port"] == "/dev/ttyUSB0"
        assert d["baud_rate"] == 9600
        assert "throughput_bytes_per_sec" in d

    def test_all_protocols(self):
        protocols = [p.value for p in SerialProtocol]
        assert "dmx512" in protocols
        assert "midi_din" in protocols
        assert "modbus_rtu" in protocols
