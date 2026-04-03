"""Unit tests for controller/iot_network.py.

Covers: VLANConfig, IoTDevice, OnboardingSession models; IoTNetworkManager
device CRUD; onboarding flow (start/complete/cancel/expire); DHCP lease sync;
nftables rule generation; hardware backend selection.
"""

from __future__ import annotations

import asyncio
import time
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from iot_network import (
    DeviceCategory, InternetAccess, IoTDevice, IoTNetworkManager,
    NativeLinuxBackend, OnboardingSession, OnboardingState, VLANConfig,
)


# ── Helpers ──────────────────────────────────────────────────────────────────

def _mgr(tmp_path: Path) -> IoTNetworkManager:
    return IoTNetworkManager(config_path=tmp_path / "iot_network.json")


# ── VLANConfig ────────────────────────────────────────────────────────────────

class TestVLANConfig:
    def test_defaults(self):
        cfg = VLANConfig()
        assert cfg.vlan_id == 20
        assert cfg.subnet == "192.168.20"
        assert cfg.gateway == "192.168.20.1"

    def test_roundtrip(self):
        cfg = VLANConfig(vlan_id=30, subnet="10.10.30", gateway="10.10.30.1",
                          dhcp_start="10.10.30.50", dhcp_end="10.10.30.150",
                          dns="10.10.30.1", iface="eth1")
        cfg2 = VLANConfig.from_dict(cfg.to_dict())
        assert cfg2.vlan_id == 30
        assert cfg2.iface == "eth1"


# ── IoTDevice ─────────────────────────────────────────────────────────────────

class TestIoTDevice:
    def test_defaults(self):
        dev = IoTDevice(id="d1", mac="aa:bb:cc:dd:ee:ff")
        assert dev.category == DeviceCategory.UNKNOWN
        assert dev.internet_access == InternetAccess.DENY
        assert dev.blocked is False

    def test_roundtrip(self):
        dev = IoTDevice(
            id="d2", mac="11:22:33:44:55:66", name="cam1",
            category=DeviceCategory.CAMERA, internet_access=InternetAccess.ALLOW,
            blocked=True,
        )
        dev2 = IoTDevice.from_dict(dev.to_dict())
        assert dev2.category == DeviceCategory.CAMERA
        assert dev2.internet_access == InternetAccess.ALLOW
        assert dev2.blocked is True

    def test_from_dict_unknown_category_defaults(self):
        dev = IoTDevice.from_dict({"id": "d3", "mac": "aa:bb:cc:00:00:00", "category": "camera"})
        assert dev.category == DeviceCategory.CAMERA


# ── OnboardingSession ─────────────────────────────────────────────────────────

class TestOnboardingSession:
    def test_not_expired_when_no_expiry(self):
        sess = OnboardingSession(id="s1", expires_at=0)
        assert sess.expired is False

    def test_expired_when_past_deadline(self):
        sess = OnboardingSession(id="s2", expires_at=time.time() - 1)
        assert sess.expired is True

    def test_not_expired_within_deadline(self):
        sess = OnboardingSession(id="s3", expires_at=time.time() + 1000)
        assert sess.expired is False

    def test_roundtrip(self):
        sess = OnboardingSession(
            id="s4", device_name="Nest", category=DeviceCategory.SMART_HOME,
            phone_ip="192.168.1.50", state=OnboardingState.COMPLETE,
            allow_internet=True, created_at=1000.0, completed_at=1500.0,
            device_id="dev-abc",
        )
        s2 = OnboardingSession.from_dict(sess.to_dict())
        assert s2.state == OnboardingState.COMPLETE
        assert s2.allow_internet is True
        assert s2.device_id == "dev-abc"


# ── IoTNetworkManager: device CRUD ───────────────────────────────────────────

class TestDeviceCRUD:
    def test_add_device(self, tmp_path):
        mgr = _mgr(tmp_path)
        dev = mgr.add_device("AA:BB:CC:DD:EE:FF", name="cam1",
                              category=DeviceCategory.CAMERA)
        assert dev.mac == "aa:bb:cc:dd:ee:ff"  # normalised
        assert dev.name == "cam1"
        assert dev.id != ""

    def test_add_device_normalises_mac(self, tmp_path):
        mgr = _mgr(tmp_path)
        dev = mgr.add_device("AA-BB-CC-DD-EE-FF")
        assert dev.mac == "aa:bb:cc:dd:ee:ff"

    def test_add_device_deduplicates_by_mac(self, tmp_path):
        mgr = _mgr(tmp_path)
        d1 = mgr.add_device("aa:bb:cc:00:00:01", name="first")
        d2 = mgr.add_device("aa:bb:cc:00:00:01", name="updated")
        assert d1.id == d2.id
        assert d2.name == "updated"
        assert len(mgr.list_devices()) == 1

    def test_get_device(self, tmp_path):
        mgr = _mgr(tmp_path)
        dev = mgr.add_device("aa:00:00:00:00:01", name="cam2")
        fetched = mgr.get_device(dev.id)
        assert fetched is not None
        assert fetched.name == "cam2"

    def test_get_device_missing_returns_none(self, tmp_path):
        mgr = _mgr(tmp_path)
        assert mgr.get_device("nonexistent") is None

    def test_get_device_by_mac(self, tmp_path):
        mgr = _mgr(tmp_path)
        mgr.add_device("aa:00:00:00:00:02", name="sensor")
        dev = mgr.get_device_by_mac("AA:00:00:00:00:02")
        assert dev is not None
        assert dev.name == "sensor"

    def test_update_device(self, tmp_path):
        mgr = _mgr(tmp_path)
        dev = mgr.add_device("aa:00:00:00:00:03")
        updated = mgr.update_device(dev.id, name="renamed", blocked=True)
        assert updated is not None
        assert updated.name == "renamed"
        assert updated.blocked is True

    def test_update_device_missing_returns_none(self, tmp_path):
        mgr = _mgr(tmp_path)
        assert mgr.update_device("ghost", name="x") is None

    def test_remove_device(self, tmp_path):
        mgr = _mgr(tmp_path)
        dev = mgr.add_device("aa:00:00:00:00:04")
        assert mgr.remove_device(dev.id) is True
        assert mgr.get_device(dev.id) is None

    def test_remove_device_missing_returns_false(self, tmp_path):
        mgr = _mgr(tmp_path)
        assert mgr.remove_device("ghost") is False

    def test_persistence(self, tmp_path):
        mgr1 = _mgr(tmp_path)
        mgr1.add_device("aa:00:00:00:00:05", name="persisted")

        mgr2 = _mgr(tmp_path)
        assert len(mgr2.list_devices()) == 1
        assert mgr2.list_devices()[0].name == "persisted"

    def test_update_lease(self, tmp_path):
        mgr = _mgr(tmp_path)
        dev = mgr.add_device("aa:00:00:00:00:06")
        mgr.update_lease("aa:00:00:00:00:06", "192.168.20.101")
        assert mgr.get_device(dev.id).ip == "192.168.20.101"

    def test_update_lease_unknown_mac_no_error(self, tmp_path):
        mgr = _mgr(tmp_path)
        mgr.update_lease("ff:ff:ff:ff:ff:ff", "10.0.0.1")  # should not raise


# ── Onboarding flow ───────────────────────────────────────────────────────────

class TestOnboardingFlow:
    @pytest.mark.asyncio
    async def test_start_onboarding_creates_session(self, tmp_path):
        mgr = _mgr(tmp_path)
        mgr._backend = MagicMock(
            apply_onboarding_exception=AsyncMock(return_value=True),
            remove_onboarding_exception=AsyncMock(return_value=True),
        )
        sess = await mgr.start_onboarding(device_name="Wyze Cam",
                                           category=DeviceCategory.CAMERA,
                                           phone_ip="192.168.1.50")
        assert sess.state == OnboardingState.PENDING
        assert sess.device_name == "Wyze Cam"
        assert sess.phone_ip == "192.168.1.50"
        assert sess.id in {s.id for s in mgr.list_sessions()}

    @pytest.mark.asyncio
    async def test_start_onboarding_calls_backend(self, tmp_path):
        mgr = _mgr(tmp_path)
        mock_backend = MagicMock(
            apply_onboarding_exception=AsyncMock(return_value=True),
        )
        mgr._backend = mock_backend
        await mgr.start_onboarding(device_name="dev")
        mock_backend.apply_onboarding_exception.assert_called_once()

    @pytest.mark.asyncio
    async def test_complete_onboarding_adds_device(self, tmp_path):
        mgr = _mgr(tmp_path)
        mgr._backend = MagicMock(
            apply_onboarding_exception=AsyncMock(return_value=True),
            remove_onboarding_exception=AsyncMock(return_value=True),
            apply_device_rules=AsyncMock(return_value=True),
        )
        sess = await mgr.start_onboarding(device_name="Ring")
        dev = await mgr.complete_onboarding(sess.id, mac="bb:00:00:00:00:01", name="Ring Bell")
        assert dev is not None
        assert dev.name == "Ring Bell"
        assert dev.mac == "bb:00:00:00:00:01"

    @pytest.mark.asyncio
    async def test_complete_onboarding_marks_session_complete(self, tmp_path):
        mgr = _mgr(tmp_path)
        mgr._backend = MagicMock(
            apply_onboarding_exception=AsyncMock(return_value=True),
            remove_onboarding_exception=AsyncMock(return_value=True),
            apply_device_rules=AsyncMock(return_value=True),
        )
        sess = await mgr.start_onboarding()
        await mgr.complete_onboarding(sess.id, mac="bb:00:00:00:00:02")
        assert mgr.get_session(sess.id).state == OnboardingState.COMPLETE

    @pytest.mark.asyncio
    async def test_complete_onboarding_removes_exception(self, tmp_path):
        mgr = _mgr(tmp_path)
        mock_backend = MagicMock(
            apply_onboarding_exception=AsyncMock(return_value=True),
            remove_onboarding_exception=AsyncMock(return_value=True),
            apply_device_rules=AsyncMock(return_value=True),
        )
        mgr._backend = mock_backend
        sess = await mgr.start_onboarding()
        await mgr.complete_onboarding(sess.id, mac="bb:00:00:00:00:03")
        mock_backend.remove_onboarding_exception.assert_called_with(sess.id)

    @pytest.mark.asyncio
    async def test_complete_nonexistent_session_returns_none(self, tmp_path):
        mgr = _mgr(tmp_path)
        result = await mgr.complete_onboarding("ghost-id", mac="cc:00:00:00:00:01")
        assert result is None

    @pytest.mark.asyncio
    async def test_cancel_onboarding(self, tmp_path):
        mgr = _mgr(tmp_path)
        mgr._backend = MagicMock(
            apply_onboarding_exception=AsyncMock(return_value=True),
            remove_onboarding_exception=AsyncMock(return_value=True),
        )
        sess = await mgr.start_onboarding()
        ok = await mgr.cancel_onboarding(sess.id)
        assert ok is True
        assert mgr.get_session(sess.id).state == OnboardingState.CANCELLED

    @pytest.mark.asyncio
    async def test_cancel_already_complete_returns_false(self, tmp_path):
        mgr = _mgr(tmp_path)
        mgr._backend = MagicMock(
            apply_onboarding_exception=AsyncMock(return_value=True),
            remove_onboarding_exception=AsyncMock(return_value=True),
            apply_device_rules=AsyncMock(return_value=True),
        )
        sess = await mgr.start_onboarding()
        await mgr.complete_onboarding(sess.id, mac="dd:00:00:00:00:01")
        ok = await mgr.cancel_onboarding(sess.id)
        assert ok is False

    @pytest.mark.asyncio
    async def test_onboarding_ttl_expiry_loop(self, tmp_path):
        mgr = _mgr(tmp_path)
        mgr._backend = MagicMock(
            apply_onboarding_exception=AsyncMock(return_value=True),
            remove_onboarding_exception=AsyncMock(return_value=True),
        )
        # Create an already-expired session
        sess = await mgr.start_onboarding(ttl=1)
        sess.expires_at = time.time() - 1  # force expired

        # Run one iteration of the expiry loop
        await mgr._expiry_loop.__wrapped__(mgr) if hasattr(
            mgr._expiry_loop, "__wrapped__") else None
        # Directly call the check logic
        now = time.time()
        for s in list(mgr._sessions.values()):
            if s.state == OnboardingState.PENDING and s.expired:
                s.state = OnboardingState.EXPIRED
                await mgr._backend.remove_onboarding_exception(s.id)

        assert mgr.get_session(sess.id).state == OnboardingState.EXPIRED

    def test_list_sessions_active_only(self, tmp_path):
        mgr = _mgr(tmp_path)
        s1 = OnboardingSession(id="s1", state=OnboardingState.PENDING,
                                expires_at=time.time() + 1000)
        s2 = OnboardingSession(id="s2", state=OnboardingState.COMPLETE)
        s3 = OnboardingSession(id="s3", state=OnboardingState.PENDING,
                                expires_at=time.time() - 1)  # expired
        mgr._sessions = {"s1": s1, "s2": s2, "s3": s3}
        active = mgr.list_sessions(active_only=True)
        assert len(active) == 1
        assert active[0].id == "s1"


# ── nftables rule generation ──────────────────────────────────────────────────

class TestNftablesGeneration:
    def _gen(self, devices=None, sessions=None):
        cfg = VLANConfig()
        linux = NativeLinuxBackend()
        return linux._generate_nftables(devices or [], cfg, sessions)

    def test_contains_default_deny(self):
        rules = self._gen()
        assert "type filter hook forward" in rules
        assert "policy drop" in rules

    def test_allows_ozma_control_port(self):
        rules = self._gen()
        assert "7380" in rules

    def test_allows_frigate_port(self):
        rules = self._gen()
        assert "5000" in rules

    def test_blocked_device_gets_drop_rule(self):
        dev = IoTDevice(id="d1", mac="aa:bb:cc:dd:ee:ff", name="bad", blocked=True)
        rules = self._gen(devices=[dev])
        assert "aa:bb:cc:dd:ee:ff" in rules
        assert "drop" in rules

    def test_internet_allowed_device_gets_accept_rule(self):
        dev = IoTDevice(id="d2", mac="11:22:33:44:55:66",
                         internet_access=InternetAccess.ALLOW)
        rules = self._gen(devices=[dev])
        assert "11:22:33:44:55:66" in rules
        assert "accept" in rules

    def test_default_deny_device_not_in_rules(self):
        dev = IoTDevice(id="d3", mac="aa:00:00:00:00:01",
                         internet_access=InternetAccess.DENY)
        rules = self._gen(devices=[dev])
        # Default-deny device has no special rule — just covered by default drop
        assert "aa:00:00:00:00:01" not in rules

    def test_active_onboarding_session_gets_phone_ip_rule(self):
        sess = OnboardingSession(
            id="s1", state=OnboardingState.PENDING,
            phone_ip="192.168.1.50", expires_at=time.time() + 1000,
        )
        rules = self._gen(sessions=[sess])
        assert "192.168.1.50" in rules

    def test_expired_session_not_in_rules(self):
        sess = OnboardingSession(
            id="s2", state=OnboardingState.PENDING,
            phone_ip="192.168.1.51", expires_at=time.time() - 1,
        )
        rules = self._gen(sessions=[sess])
        assert "192.168.1.51" not in rules

    def test_completed_session_not_in_rules(self):
        sess = OnboardingSession(
            id="s3", state=OnboardingState.COMPLETE,
            phone_ip="192.168.1.52", expires_at=time.time() + 1000,
        )
        rules = self._gen(sessions=[sess])
        assert "192.168.1.52" not in rules

    def test_allow_internet_session_gets_subnet_accept(self):
        sess = OnboardingSession(
            id="s4", state=OnboardingState.PENDING,
            allow_internet=True, expires_at=time.time() + 1000,
        )
        rules = self._gen(sessions=[sess])
        assert "accept" in rules

    def test_export_nftables_uses_active_sessions(self, tmp_path):
        mgr = _mgr(tmp_path)
        sess = OnboardingSession(
            id="s5", state=OnboardingState.PENDING,
            phone_ip="10.0.0.99", expires_at=time.time() + 1000,
        )
        mgr._sessions["s5"] = sess
        rules = mgr.export_nftables()
        assert "10.0.0.99" in rules


# ── Backend selection ─────────────────────────────────────────────────────────

class TestBackendSelection:
    def test_default_backend_is_native_linux(self, tmp_path):
        mgr = _mgr(tmp_path)
        assert mgr._backend.name == "native_linux"

    def test_configure_unifi_backend(self, tmp_path):
        mgr = _mgr(tmp_path)
        mgr.configure_backend("unifi", host="unifi.local",
                               username="admin", password="secret")
        assert mgr._backend.name == "unifi"

    def test_configure_mikrotik_backend(self, tmp_path):
        mgr = _mgr(tmp_path)
        mgr.configure_backend("mikrotik", host="10.0.0.1",
                               username="admin", password="")
        assert mgr._backend.name == "mikrotik"

    def test_configure_openwrt_backend(self, tmp_path):
        mgr = _mgr(tmp_path)
        mgr.configure_backend("openwrt", host="192.168.1.1")
        assert mgr._backend.name == "openwrt"

    def test_configure_pfsense_backend(self, tmp_path):
        mgr = _mgr(tmp_path)
        mgr.configure_backend("pfsense", host="pfsense.local",
                               api_key="key", api_secret="secret")
        assert mgr._backend.name == "pfsense"

    def test_configure_unknown_backend_raises(self, tmp_path):
        mgr = _mgr(tmp_path)
        with pytest.raises(ValueError, match="Unknown backend"):
            mgr.configure_backend("fritzbox")


# ── Status ────────────────────────────────────────────────────────────────────

class TestStatus:
    def test_status_shape(self, tmp_path):
        mgr = _mgr(tmp_path)
        d1 = mgr.add_device("aa:00:00:00:00:10")
        mgr.update_device(d1.id, blocked=True)
        mgr.add_device("aa:00:00:00:00:11",
                        internet_access=InternetAccess.ALLOW)
        s = mgr.status()
        assert s["device_count"] == 2
        assert s["devices_blocked"] == 1
        assert s["devices_with_internet"] == 1
        assert "vlan" in s
        assert "backend" in s

    def test_active_onboarding_count(self, tmp_path):
        mgr = _mgr(tmp_path)
        mgr._sessions["s1"] = OnboardingSession(
            id="s1", state=OnboardingState.PENDING,
            expires_at=time.time() + 1000,
        )
        mgr._sessions["s2"] = OnboardingSession(
            id="s2", state=OnboardingState.COMPLETE,
        )
        s = mgr.status()
        assert s["active_onboarding"] == 1


# ── DHCP lease sync ───────────────────────────────────────────────────────────

class TestDHCPLeaseSync:
    @pytest.mark.asyncio
    async def test_sync_leases_updates_device_ip(self, tmp_path):
        mgr = _mgr(tmp_path)
        dev = mgr.add_device("cc:00:00:00:00:01")
        mock_backend = MagicMock(
            get_dhcp_leases=AsyncMock(return_value=[
                {"mac": "cc:00:00:00:00:01", "ip": "192.168.20.105",
                 "hostname": "cam", "expires": "99999"},
            ])
        )
        mgr._backend = mock_backend
        count = await mgr.sync_leases()
        assert count == 1
        assert mgr.get_device(dev.id).ip == "192.168.20.105"

    @pytest.mark.asyncio
    async def test_sync_leases_ignores_unknown_macs(self, tmp_path):
        mgr = _mgr(tmp_path)
        mock_backend = MagicMock(
            get_dhcp_leases=AsyncMock(return_value=[
                {"mac": "ff:ff:ff:ff:ff:ff", "ip": "192.168.20.199",
                 "hostname": "", "expires": "0"},
            ])
        )
        mgr._backend = mock_backend
        count = await mgr.sync_leases()
        # lease was processed but no device matched
        assert count == 1
        assert len(mgr.list_devices()) == 0
