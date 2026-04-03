# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Unit tests for controller/auto_configure.py — V1.7 PoE subnet camera discovery.
"""

from __future__ import annotations

import asyncio
import json
import sys
import time
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

# Ensure controller package is importable
sys.path.insert(0, str(Path(__file__).parents[3] / "controller"))

from auto_configure import (
    AutoConfigureDevice,
    AutoConfigureManager,
    _CAMERA_VENDORS,
    DEFAULT_POE_SUBNET,
)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def make_state():
    state = MagicMock()
    state.events = asyncio.Queue()
    state.add_node = AsyncMock()
    return state


def make_manager(tmp_path, state=None, subnet=DEFAULT_POE_SUBNET):
    mgr = AutoConfigureManager(
        state=state or make_state(),
        poe_subnet=subnet,
        lease_file=tmp_path / "dnsmasq.leases",
        data_dir=tmp_path / "ac_data",
    )
    return mgr


# ---------------------------------------------------------------------------
# AutoConfigureDevice
# ---------------------------------------------------------------------------

class TestAutoConfigureDevice:
    def test_to_dict_roundtrip(self):
        dev = AutoConfigureDevice(
            ip="192.168.100.5",
            mac="D4859A112233",
            hostname="cam01",
            vendor="Hikvision",
            device_type="camera",
            rtsp_urls=["rtsp://192.168.100.5/"],
            onvif=True,
            http_title="Hikvision NVR",
            registered=False,
            ignored=False,
        )
        d = dev.to_dict()
        dev2 = AutoConfigureDevice.from_dict(d)
        assert dev2.ip == dev.ip
        assert dev2.mac == dev.mac
        assert dev2.vendor == dev.vendor
        assert dev2.device_type == dev.device_type
        assert dev2.rtsp_urls == dev.rtsp_urls
        assert dev2.onvif == dev.onvif
        assert dev2.http_title == dev.http_title

    def test_from_dict_missing_optional_fields(self):
        dev = AutoConfigureDevice.from_dict({"ip": "10.0.0.1", "mac": "AABBCC112233"})
        assert dev.ip == "10.0.0.1"
        assert dev.device_type == "unknown"
        assert dev.rtsp_urls == []
        assert not dev.onvif
        assert not dev.registered
        assert not dev.ignored

    def test_defaults(self):
        dev = AutoConfigureDevice(ip="1.2.3.4", mac="")
        assert dev.device_type == "unknown"
        assert dev.rtsp_urls == []
        assert not dev.onvif
        assert not dev.ignored
        assert not dev.registered
        assert dev.first_seen > 0
        assert dev.last_seen > 0


# ---------------------------------------------------------------------------
# MAC vendor lookup
# ---------------------------------------------------------------------------

class TestVendorLookup:
    def test_hikvision_oui(self, tmp_path):
        mgr = make_manager(tmp_path)
        assert mgr._lookup_vendor("D4:85:9A:11:22:33") == "Hikvision"

    def test_dahua_oui(self, tmp_path):
        mgr = make_manager(tmp_path)
        assert mgr._lookup_vendor("BC:51:41:AA:BB:CC") == "Dahua"

    def test_axis_oui(self, tmp_path):
        mgr = make_manager(tmp_path)
        assert mgr._lookup_vendor("00:0F:18:AA:BB:CC") == "Axis"

    def test_unknown_oui(self, tmp_path):
        mgr = make_manager(tmp_path)
        assert mgr._lookup_vendor("AA:BB:CC:11:22:33") == ""

    def test_empty_mac(self, tmp_path):
        mgr = make_manager(tmp_path)
        assert mgr._lookup_vendor("") == ""

    def test_no_separator(self, tmp_path):
        mgr = make_manager(tmp_path)
        # Uppercase without colons — as stored in _devices
        assert mgr._lookup_vendor("EC4D47AABBCC") == "Reolink"


# ---------------------------------------------------------------------------
# Lease file parsing
# ---------------------------------------------------------------------------

class TestLeaseFileParsing:
    def test_parses_valid_entries(self, tmp_path):
        lease = tmp_path / "leases"
        lease.write_text(
            "1700000000 d4:85:9a:11:22:33 192.168.100.5 cam01 *\n"
            "1700000001 bc:51:41:aa:bb:cc 192.168.100.6 cam02 *\n"
        )
        mgr = AutoConfigureManager(
            poe_subnet="192.168.100",
            lease_file=lease,
            data_dir=tmp_path / "data",
        )
        result = mgr._parse_lease_file()
        assert result["192.168.100.5"] == "D4859A112233"
        assert result["192.168.100.6"] == "BC5141AABBCC"

    def test_empty_file(self, tmp_path):
        lease = tmp_path / "leases"
        lease.write_text("")
        mgr = AutoConfigureManager(lease_file=lease, data_dir=tmp_path / "data")
        assert mgr._parse_lease_file() == {}

    def test_missing_file(self, tmp_path):
        mgr = AutoConfigureManager(
            lease_file=tmp_path / "nonexistent.leases",
            data_dir=tmp_path / "data",
        )
        assert mgr._parse_lease_file() == {}

    def test_short_lines_skipped(self, tmp_path):
        lease = tmp_path / "leases"
        lease.write_text("badline\n1700000000 aa:bb:cc:dd:ee:ff 10.0.0.1\n")
        mgr = AutoConfigureManager(lease_file=lease, data_dir=tmp_path / "data")
        result = mgr._parse_lease_file()
        assert "10.0.0.1" in result


# ---------------------------------------------------------------------------
# ARP table parsing
# ---------------------------------------------------------------------------

class TestARPTableParsing:
    def test_parses_complete_entries(self, tmp_path):
        arp_content = (
            "IP address       HW type  Flags       HW address            Mask     Device\n"
            "192.168.100.10   0x1      0x2         d4:85:9a:11:22:33     *        eth0\n"
            "192.168.100.11   0x1      0x0         00:00:00:00:00:00     *        eth0\n"  # incomplete
        )
        mgr = make_manager(tmp_path)
        with patch.object(Path, "exists", return_value=True), \
             patch.object(Path, "read_text", return_value=arp_content):
            result = mgr._parse_arp_table()
        assert "192.168.100.10" in result
        assert result["192.168.100.10"] == "D4859A112233"
        # Incomplete entry (0x0 flags) should not appear
        assert "192.168.100.11" not in result

    def test_missing_proc_arp(self, tmp_path):
        mgr = make_manager(tmp_path)
        with patch.object(Path, "exists", return_value=False):
            result = mgr._parse_arp_table()
        assert result == {}


# ---------------------------------------------------------------------------
# Subnet filtering in _do_scan
# ---------------------------------------------------------------------------

class TestSubnetFiltering:
    @pytest.mark.asyncio
    async def test_filters_to_subnet(self, tmp_path):
        mgr = make_manager(tmp_path, subnet="192.168.100")
        # Inject devices from different subnets via _parse_lease_file mock
        lease = tmp_path / "leases"
        lease.write_text(
            "1700000000 d4:85:9a:11:22:33 192.168.100.5 cam01 *\n"
            "1700000001 aa:bb:cc:11:22:33 10.0.0.1 other *\n"
        )
        mgr._lease_file = lease
        with patch.object(mgr, "_arp_scan", AsyncMock(return_value={})):
            new_ips = await mgr._do_scan()
        assert "192.168.100.5" in new_ips
        assert "10.0.0.1" not in new_ips

    @pytest.mark.asyncio
    async def test_already_known_ips_not_returned(self, tmp_path):
        mgr = make_manager(tmp_path)
        mgr._devices["192.168.100.5"] = AutoConfigureDevice(ip="192.168.100.5", mac="")
        lease = tmp_path / "leases"
        lease.write_text("1700000000 d4:85:9a:11:22:33 192.168.100.5 cam01 *\n")
        mgr._lease_file = lease
        with patch.object(mgr, "_arp_scan", AsyncMock(return_value={})):
            new_ips = await mgr._do_scan()
        assert "192.168.100.5" not in new_ips


# ---------------------------------------------------------------------------
# Fingerprinting / classification
# ---------------------------------------------------------------------------

class TestFingerprinting:
    @pytest.mark.asyncio
    async def test_rtsp_port_closed_returns_empty(self, tmp_path):
        mgr = make_manager(tmp_path)
        with patch("asyncio.open_connection", side_effect=OSError("refused")):
            urls = await mgr._probe_rtsp("192.168.100.5")
        assert urls == []

    @pytest.mark.asyncio
    async def test_rtsp_port_open_returns_candidates(self, tmp_path):
        mgr = make_manager(tmp_path)
        writer_mock = MagicMock()
        writer_mock.close = MagicMock()
        writer_mock.wait_closed = AsyncMock()
        with patch("asyncio.open_connection", AsyncMock(return_value=(MagicMock(), writer_mock))):
            urls = await mgr._probe_rtsp("192.168.100.5")
        assert len(urls) >= 1
        assert all(u.startswith("rtsp://192.168.100.5") for u in urls)

    @pytest.mark.asyncio
    async def test_http_title_extracted(self, tmp_path):
        mgr = make_manager(tmp_path)
        html = b"<html><head><title>Hikvision NVR</title></head></html>"

        class FakeResp:
            status = 200
            def read(self, n): return html

        with patch("urllib.request.urlopen", return_value=FakeResp()):
            title = await mgr._probe_http("192.168.100.5")
        assert title == "Hikvision NVR"

    @pytest.mark.asyncio
    async def test_http_probe_failure_returns_empty(self, tmp_path):
        mgr = make_manager(tmp_path)
        with patch("urllib.request.urlopen", side_effect=Exception("timeout")):
            title = await mgr._probe_http("192.168.100.5")
        assert title == ""

    @pytest.mark.asyncio
    async def test_classify_camera_by_rtsp(self, tmp_path):
        mgr = make_manager(tmp_path)
        mgr._devices["192.168.100.5"] = AutoConfigureDevice(ip="192.168.100.5", mac="")
        with patch.object(mgr, "_probe_rtsp", AsyncMock(return_value=["rtsp://192.168.100.5/"])), \
             patch.object(mgr, "_probe_onvif", AsyncMock(return_value=False)), \
             patch.object(mgr, "_probe_http", AsyncMock(return_value="")), \
             patch.object(mgr, "_resolve_hostname", AsyncMock(return_value="")):
            dev = await mgr._fingerprint("192.168.100.5", "")
        assert dev.device_type == "camera"

    @pytest.mark.asyncio
    async def test_classify_camera_by_vendor(self, tmp_path):
        mgr = make_manager(tmp_path)
        mgr._devices["192.168.100.5"] = AutoConfigureDevice(ip="192.168.100.5", mac="D4859A112233")
        with patch.object(mgr, "_probe_rtsp", AsyncMock(return_value=[])), \
             patch.object(mgr, "_probe_onvif", AsyncMock(return_value=False)), \
             patch.object(mgr, "_probe_http", AsyncMock(return_value="")), \
             patch.object(mgr, "_resolve_hostname", AsyncMock(return_value="")):
            dev = await mgr._fingerprint("192.168.100.5", "D4859A112233")
        assert dev.device_type == "camera"
        assert dev.vendor == "Hikvision"

    @pytest.mark.asyncio
    async def test_classify_nvr_by_http_title(self, tmp_path):
        mgr = make_manager(tmp_path)
        with patch.object(mgr, "_probe_rtsp", AsyncMock(return_value=[])), \
             patch.object(mgr, "_probe_onvif", AsyncMock(return_value=False)), \
             patch.object(mgr, "_probe_http", AsyncMock(return_value="Network DVR Recorder")), \
             patch.object(mgr, "_resolve_hostname", AsyncMock(return_value="")):
            dev = await mgr._fingerprint("192.168.100.7", "AABBCC112233")
        assert dev.device_type == "nvr"

    @pytest.mark.asyncio
    async def test_classify_unknown(self, tmp_path):
        mgr = make_manager(tmp_path)
        with patch.object(mgr, "_probe_rtsp", AsyncMock(return_value=[])), \
             patch.object(mgr, "_probe_onvif", AsyncMock(return_value=False)), \
             patch.object(mgr, "_probe_http", AsyncMock(return_value="My Router")), \
             patch.object(mgr, "_resolve_hostname", AsyncMock(return_value="")):
            dev = await mgr._fingerprint("192.168.100.1", "AABBCC112233")
        assert dev.device_type == "unknown"


# ---------------------------------------------------------------------------
# Public API
# ---------------------------------------------------------------------------

class TestPublicAPI:
    def test_list_devices_returns_dicts(self, tmp_path):
        mgr = make_manager(tmp_path)
        mgr._devices["192.168.100.5"] = AutoConfigureDevice(ip="192.168.100.5", mac="D4859A112233")
        result = mgr.list_devices()
        assert isinstance(result, list)
        assert result[0]["ip"] == "192.168.100.5"

    def test_get_device_found(self, tmp_path):
        mgr = make_manager(tmp_path)
        mgr._devices["192.168.100.5"] = AutoConfigureDevice(ip="192.168.100.5", mac="")
        dev = mgr.get_device("192.168.100.5")
        assert dev is not None
        assert dev.ip == "192.168.100.5"

    def test_get_device_not_found(self, tmp_path):
        mgr = make_manager(tmp_path)
        assert mgr.get_device("10.0.0.1") is None

    def test_ignore_device(self, tmp_path):
        mgr = make_manager(tmp_path)
        mgr._devices["192.168.100.5"] = AutoConfigureDevice(ip="192.168.100.5", mac="")
        mgr.ignore_device("192.168.100.5")
        assert mgr._devices["192.168.100.5"].ignored is True

    def test_ignore_unknown_device_noop(self, tmp_path):
        mgr = make_manager(tmp_path)
        mgr.ignore_device("10.0.0.99")  # Should not raise

    def test_unignore_device(self, tmp_path):
        mgr = make_manager(tmp_path)
        mgr._devices["192.168.100.5"] = AutoConfigureDevice(ip="192.168.100.5", mac="", ignored=True)
        mgr.unignore_device("192.168.100.5")
        assert mgr._devices["192.168.100.5"].ignored is False

    @pytest.mark.asyncio
    async def test_register_device_creates_node(self, tmp_path):
        state = make_state()
        mgr = make_manager(tmp_path, state=state)
        mgr._devices["192.168.100.5"] = AutoConfigureDevice(
            ip="192.168.100.5", mac="D4859A112233",
            vendor="Hikvision", device_type="camera",
            rtsp_urls=["rtsp://192.168.100.5/"],
        )
        result = await mgr.register_device("192.168.100.5", "cam01")
        assert result["ok"] is True
        assert "node_id" in result
        state.add_node.assert_called_once()
        node_arg = state.add_node.call_args[0][0]
        assert node_arg.machine_class == "camera"
        assert node_arg.host == "192.168.100.5"
        assert "rtsp" in node_arg.capabilities

    @pytest.mark.asyncio
    async def test_register_device_marks_registered(self, tmp_path):
        state = make_state()
        mgr = make_manager(tmp_path, state=state)
        mgr._devices["192.168.100.5"] = AutoConfigureDevice(ip="192.168.100.5", mac="")
        await mgr.register_device("192.168.100.5", "cam01")
        assert mgr._devices["192.168.100.5"].registered is True

    @pytest.mark.asyncio
    async def test_register_unknown_device_returns_error(self, tmp_path):
        mgr = make_manager(tmp_path)
        result = await mgr.register_device("10.0.0.99", "cam99")
        assert result["ok"] is False

    @pytest.mark.asyncio
    async def test_scan_now_returns_new_devices(self, tmp_path):
        mgr = make_manager(tmp_path)
        new_dev = AutoConfigureDevice(
            ip="192.168.100.8", mac="EC4D47AABBCC",
            device_type="camera", rtsp_urls=["rtsp://192.168.100.8/"],
        )
        with patch.object(mgr, "_do_scan", AsyncMock(return_value=["192.168.100.8"])), \
             patch.object(mgr, "_fingerprint", AsyncMock(return_value=new_dev)):
            results = await mgr.scan_now()
        assert len(results) == 1
        assert results[0]["ip"] == "192.168.100.8"

    @pytest.mark.asyncio
    async def test_scan_now_ignored_devices_excluded(self, tmp_path):
        mgr = make_manager(tmp_path)
        mgr._devices["192.168.100.8"] = AutoConfigureDevice(
            ip="192.168.100.8", mac="", ignored=True
        )
        ignored_dev = AutoConfigureDevice(ip="192.168.100.8", mac="", ignored=True)
        with patch.object(mgr, "_do_scan", AsyncMock(return_value=["192.168.100.8"])), \
             patch.object(mgr, "_fingerprint", AsyncMock(return_value=ignored_dev)):
            results = await mgr.scan_now()
        assert results == []


# ---------------------------------------------------------------------------
# Persistence
# ---------------------------------------------------------------------------

class TestPersistence:
    def test_save_and_load(self, tmp_path):
        mgr = make_manager(tmp_path)
        mgr._devices["192.168.100.5"] = AutoConfigureDevice(
            ip="192.168.100.5", mac="D4859A112233",
            vendor="Hikvision", device_type="camera",
            rtsp_urls=["rtsp://192.168.100.5/"],
            registered=True,
        )
        mgr._save()

        mgr2 = make_manager(tmp_path)
        assert "192.168.100.5" in mgr2._devices
        dev = mgr2._devices["192.168.100.5"]
        assert dev.vendor == "Hikvision"
        assert dev.registered is True
        assert dev.rtsp_urls == ["rtsp://192.168.100.5/"]

    def test_save_file_permissions(self, tmp_path):
        mgr = make_manager(tmp_path)
        mgr._devices["192.168.100.5"] = AutoConfigureDevice(ip="192.168.100.5", mac="")
        mgr._save()
        p = tmp_path / "ac_data" / "devices.json"
        assert p.exists()
        assert oct(p.stat().st_mode)[-3:] == "600"

    def test_load_missing_file_no_error(self, tmp_path):
        mgr = make_manager(tmp_path)  # No devices.json yet
        assert mgr._devices == {}

    def test_load_corrupt_file_no_error(self, tmp_path):
        data_dir = tmp_path / "ac_data"
        data_dir.mkdir()
        (data_dir / "devices.json").write_text("not valid json {{{")
        mgr = make_manager(tmp_path)
        assert mgr._devices == {}


# ---------------------------------------------------------------------------
# Event firing
# ---------------------------------------------------------------------------

class TestEventFiring:
    @pytest.mark.asyncio
    async def test_fires_device_discovered_event(self, tmp_path):
        state = make_state()
        mgr = make_manager(tmp_path, state=state)
        dev = AutoConfigureDevice(
            ip="192.168.100.5", mac="D4859A112233",
            vendor="Hikvision", device_type="camera",
            rtsp_urls=["rtsp://192.168.100.5/"], onvif=True,
        )
        await mgr._fire_event(dev)
        assert not state.events.empty()
        evt = await state.events.get()
        assert evt["type"] == "device_discovered"
        assert evt["ip"] == "192.168.100.5"
        assert evt["vendor"] == "Hikvision"
        assert evt["rtsp_count"] == 1
        assert evt["onvif"] is True

    @pytest.mark.asyncio
    async def test_no_event_without_state(self, tmp_path):
        mgr = AutoConfigureManager(state=None, data_dir=tmp_path / "data")
        dev = AutoConfigureDevice(ip="192.168.100.5", mac="")
        # Should not raise
        await mgr._fire_event(dev)


# ---------------------------------------------------------------------------
# Lifecycle
# ---------------------------------------------------------------------------

class TestLifecycle:
    @pytest.mark.asyncio
    async def test_start_creates_scan_task(self, tmp_path):
        mgr = make_manager(tmp_path)
        with patch.object(mgr, "_scan_loop", AsyncMock()):
            await mgr.start()
            assert len(mgr._tasks) == 1
            await mgr.stop()

    @pytest.mark.asyncio
    async def test_stop_cancels_tasks(self, tmp_path):
        mgr = make_manager(tmp_path)
        cancelled = []

        async def slow_loop():
            try:
                await asyncio.sleep(9999)
            except asyncio.CancelledError:
                cancelled.append(True)
                raise

        task = asyncio.create_task(slow_loop())
        mgr._tasks.append(task)
        # Yield so the task actually starts and reaches its first await
        await asyncio.sleep(0)
        await mgr.stop()
        assert cancelled

    @pytest.mark.asyncio
    async def test_start_creates_data_dir(self, tmp_path):
        data_dir = tmp_path / "nested" / "ac_data"
        mgr = AutoConfigureManager(data_dir=data_dir)
        with patch.object(mgr, "_scan_loop", AsyncMock()):
            await mgr.start()
            await mgr.stop()
        assert data_dir.exists()
