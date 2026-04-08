#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Unit tests for MDMBridgeManager — mobile device management abstraction.
"""

import asyncio
import base64
import json
import os
import sys
import time
import unittest
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

sys.path.insert(0, str(Path(__file__).parent.parent.parent.parent / "controller"))

from mdm_bridge import (
    MDMBridgeManager, MDMConfig, ManagedDevice,
    build_wg_config, build_ios_mobileconfig,
    MOBILE_WG_SUBNET, MOBILE_WG_PORT,
    _parse_google_mobile, _parse_google_chrome,
    _parse_intune_device,
    _parse_jamf_computer, _parse_jamf_mobile,
)


def _make_device(**kwargs) -> ManagedDevice:
    defaults = dict(
        id="dev-001",
        provider="intune",
        user_email="alice@example.com",
        name="Alice's MacBook",
        platform="macos",
        model="MacBookPro18,1",
        serial="C02XXXX",
        os_version="14.3",
        enrolled_at=time.time() - 86400,
        last_sync_at=time.time(),
        compliance_state="compliant",
        encrypted=True,
        screen_lock=True,
        management_state="managed",
    )
    defaults.update(kwargs)
    return ManagedDevice(**defaults)


def _mgr(tmp: Path, config: MDMConfig | None = None) -> MDMBridgeManager:
    return MDMBridgeManager(tmp, config=config)


# ── ManagedDevice model ───────────────────────────────────────────────────────

class TestManagedDeviceModel(unittest.TestCase):
    def test_to_dict_excludes_private_key_by_default(self):
        d = _make_device(vpn_private_key="secret", vpn_public_key="public", vpn_ip="10.200.250.1")
        out = d.to_dict()
        self.assertNotIn("vpn_private_key", out)
        self.assertIn("vpn_public_key", out)
        self.assertIn("vpn_ip", out)

    def test_to_dict_include_private_key(self):
        d = _make_device(vpn_private_key="secret")
        out = d.to_dict(include_private_key=True)
        self.assertEqual(out["vpn_private_key"], "secret")

    def test_roundtrip_serialization(self):
        d = _make_device(vpn_public_key="pub123", vpn_ip="10.200.250.5", vpn_private_key="priv")
        restored = ManagedDevice.from_dict(d.to_dict(include_private_key=True))
        self.assertEqual(restored.id, d.id)
        self.assertEqual(restored.user_email, d.user_email)
        self.assertEqual(restored.platform, d.platform)
        self.assertEqual(restored.vpn_private_key, d.vpn_private_key)
        self.assertEqual(restored.vpn_public_key, d.vpn_public_key)
        self.assertEqual(restored.vpn_ip, d.vpn_ip)

    def test_from_dict_defaults_for_missing_fields(self):
        d = ManagedDevice.from_dict({"id": "x", "provider": "google",
                                      "user_email": "", "name": "", "platform": "android"})
        self.assertEqual(d.compliance_state, "unknown")
        self.assertFalse(d.encrypted)
        self.assertFalse(d.vpn_profile_pushed)
        self.assertEqual(d.vpn_ip, "")


# ── MDMConfig serialization ───────────────────────────────────────────────────

class TestMDMConfig(unittest.TestCase):
    def test_defaults(self):
        cfg = MDMConfig()
        self.assertEqual(cfg.provider, "")
        self.assertEqual(cfg.sync_interval_seconds, 900)

    def test_to_dict_roundtrip(self):
        cfg = MDMConfig(
            provider="intune",
            intune_tenant_id="tenant-123",
            intune_client_id="client-456",
            intune_client_secret_env="INTUNE_SECRET",
            wg_endpoint="vpn.company.com:51821",
            wg_server_public_key="serverpub",
            wg_dns="10.200.1.1",
            sync_interval_seconds=300,
        )
        d = cfg.to_dict()
        restored = MDMConfig.from_dict(d)
        self.assertEqual(restored.provider, "intune")
        self.assertEqual(restored.intune_tenant_id, "tenant-123")
        self.assertEqual(restored.wg_endpoint, "vpn.company.com:51821")
        self.assertEqual(restored.sync_interval_seconds, 300)

    def test_google_fields(self):
        cfg = MDMConfig(
            provider="google",
            google_admin_email="admin@company.com",
            google_service_account_json_env="GOOGLE_SA_JSON",
            google_customer_id="C01abc",
        )
        d = cfg.to_dict()
        restored = MDMConfig.from_dict(d)
        self.assertEqual(restored.google_admin_email, "admin@company.com")
        self.assertEqual(restored.google_customer_id, "C01abc")

    def test_jamf_fields(self):
        cfg = MDMConfig(
            provider="jamf",
            jamf_base_url="https://company.jamfcloud.com",
            jamf_client_id="jamf-client",
            jamf_client_secret_env="JAMF_SECRET",
        )
        d = cfg.to_dict()
        restored = MDMConfig.from_dict(d)
        self.assertEqual(restored.jamf_base_url, "https://company.jamfcloud.com")


# ── WireGuard profile builders ────────────────────────────────────────────────

class TestBuildWgConfig(unittest.TestCase):
    def _config(self, **kwargs):
        return build_wg_config(
            private_key="PRIV=",
            device_ip="10.200.250.3",
            server_public_key="PUB=",
            endpoint="vpn.example.com:51821",
            **kwargs,
        )

    def test_contains_interface_and_peer(self):
        cfg = self._config()
        self.assertIn("[Interface]", cfg)
        self.assertIn("[Peer]", cfg)

    def test_private_key_present(self):
        cfg = self._config()
        self.assertIn("PrivateKey = PRIV=", cfg)

    def test_address_uses_24_prefix(self):
        cfg = self._config()
        self.assertIn("Address = 10.200.250.3/24", cfg)

    def test_endpoint_present(self):
        cfg = self._config()
        self.assertIn("Endpoint = vpn.example.com:51821", cfg)

    def test_dns_included_when_provided(self):
        cfg = self._config(dns="10.200.1.1")
        self.assertIn("DNS = 10.200.1.1", cfg)

    def test_dns_omitted_when_empty(self):
        cfg = self._config(dns="")
        self.assertNotIn("DNS", cfg)

    def test_keepalive(self):
        cfg = self._config()
        self.assertIn("PersistentKeepalive = 25", cfg)

    def test_custom_allowed_ips(self):
        cfg = self._config(allowed_ips="0.0.0.0/0")
        self.assertIn("AllowedIPs = 0.0.0.0/0", cfg)


class TestBuildIosMobileconfig(unittest.TestCase):
    def _profile(self, **kwargs):
        return build_ios_mobileconfig(
            profile_name="Ozma VPN",
            private_key="PRIV=",
            device_ip="10.200.250.4",
            server_public_key="PUB=",
            endpoint="vpn.example.com:51821",
            **kwargs,
        )

    def test_is_xml_plist(self):
        p = self._profile()
        self.assertIn("<?xml", p)
        self.assertIn("<plist", p)

    def test_contains_wireguard_type(self):
        p = self._profile()
        self.assertIn("com.wireguard.", p)

    def test_private_key_in_profile(self):
        p = self._profile()
        wg = self._decode_wg_payload(p)
        self.assertIn("PRIV=", wg)

    def test_endpoint_in_profile(self):
        p = self._profile()
        wg = self._decode_wg_payload(p)
        self.assertIn("vpn.example.com:51821", wg)

    def _decode_wg_payload(self, profile: str) -> str:
        """Decode base64 WireGuard config embedded inside the mobileconfig <data> element."""
        import re
        m = re.search(r"<data>([\s\S]+?)</data>", profile)
        self.assertIsNotNone(m, "No <data> element found in mobileconfig")
        return base64.b64decode(m.group(1).strip()).decode()

    def test_device_ip_in_profile(self):
        p = self._profile()
        wg = self._decode_wg_payload(p)
        self.assertIn("10.200.250.4", wg)

    def test_dns_included_when_provided(self):
        p = self._profile(dns="10.200.1.1")
        wg = self._decode_wg_payload(p)
        self.assertIn("10.200.1.1", wg)


# ── Provider parsers ──────────────────────────────────────────────────────────

class TestGoogleMobileParsing(unittest.TestCase):
    def _raw(self, **kwargs):
        base = {
            "deviceId": "google-mob-001",
            "email": ["bob@company.com"],
            "name": "Bob's Pixel 8",
            "model": "Pixel 8",
            "os": "Android 14",
            "serialNumber": "SN123",
            "status": "APPROVED",
            "encryptionStatus": "ENCRYPTED",
            "devicePasswordStatus": "ACTIVE",
            "firstSync": "2024-01-15T12:00:00.000Z",
            "lastSync": "2024-04-01T09:00:00.000Z",
            "type": "ANDROID",
        }
        base.update(kwargs)
        return base

    def test_basic_fields(self):
        d = _parse_google_mobile(self._raw())
        self.assertEqual(d.id, "google-mob-001")
        self.assertEqual(d.user_email, "bob@company.com")
        self.assertEqual(d.platform, "android")
        self.assertEqual(d.provider, "google")

    def test_ios_type(self):
        d = _parse_google_mobile(self._raw(type="IOS", os="iOS 17"))
        self.assertEqual(d.platform, "ios")

    def test_encrypted_flag(self):
        d = _parse_google_mobile(self._raw(encryptionStatus="ENCRYPTED"))
        self.assertTrue(d.encrypted)
        d2 = _parse_google_mobile(self._raw(encryptionStatus="ENCRYPTION_UNSUPPORTED"))
        self.assertFalse(d2.encrypted)

    def test_compliance_approved(self):
        d = _parse_google_mobile(self._raw(status="APPROVED"))
        self.assertEqual(d.compliance_state, "compliant")

    def test_compliance_not_approved(self):
        # Non-APPROVED statuses (BLOCKED, PENDING) map to unknown (not actively compliant)
        d = _parse_google_mobile(self._raw(status="BLOCKED"))
        self.assertNotEqual(d.compliance_state, "compliant")

    def test_empty_email_list(self):
        d = _parse_google_mobile(self._raw(email=[]))
        self.assertEqual(d.user_email, "")


class TestGoogleChromeParsing(unittest.TestCase):
    def _raw(self, **kwargs):
        base = {
            "deviceId": "chrome-001",
            "annotatedUser": "charlie@company.com",
            "annotatedAssetId": "CB-001",
            "model": "HP Chromebook 14",
            "serialNumber": "5CD123",
            "status": "ACTIVE",
            "osVersion": "120.0",
            "enrollmentTime": "2024-02-01T08:00:00.000Z",
            "lastSync": "2024-04-01T10:00:00.000Z",
        }
        base.update(kwargs)
        return base

    def test_platform_is_chromeos(self):
        d = _parse_google_chrome(self._raw())
        self.assertEqual(d.platform, "chromeos")

    def test_active_is_compliant(self):
        d = _parse_google_chrome(self._raw(status="ACTIVE"))
        self.assertEqual(d.compliance_state, "compliant")

    def test_deprovisioned_is_not_compliant(self):
        d = _parse_google_chrome(self._raw(status="DEPROVISIONED"))
        self.assertNotEqual(d.compliance_state, "compliant")


class TestIntuneParsing(unittest.TestCase):
    def _raw(self, **kwargs):
        base = {
            "id": "intune-001",
            "userPrincipalName": "dave@company.com",
            "deviceName": "DAVE-LAPTOP",
            "model": "Surface Pro 9",
            "serialNumber": "SN456",
            "osVersion": "Windows 11 23H2",
            "operatingSystem": "Windows",
            "enrolledDateTime": "2024-03-01T12:00:00Z",
            "lastSyncDateTime": "2024-04-01T11:00:00Z",
            "complianceState": "compliant",
            "isEncrypted": True,
            "isSupervised": False,
            "passcodeEnabled": True,
            "managementState": "managed",
        }
        base.update(kwargs)
        return base

    def test_basic_fields(self):
        d = _parse_intune_device(self._raw())
        self.assertEqual(d.id, "intune-001")
        self.assertEqual(d.user_email, "dave@company.com")
        self.assertEqual(d.platform, "windows")
        self.assertEqual(d.provider, "intune")

    def test_ios_platform(self):
        d = _parse_intune_device(self._raw(operatingSystem="iOS"))
        self.assertEqual(d.platform, "ios")

    def test_android_platform(self):
        d = _parse_intune_device(self._raw(operatingSystem="Android"))
        self.assertEqual(d.platform, "android")

    def test_linux_unknown_platform(self):
        # Unrecognised OS falls through to "unknown"
        d = _parse_intune_device(self._raw(operatingSystem="Linux"))
        self.assertEqual(d.platform, "unknown")

    def test_compliant_state(self):
        d = _parse_intune_device(self._raw(complianceState="compliant"))
        self.assertEqual(d.compliance_state, "compliant")

    def test_noncompliant_state(self):
        d = _parse_intune_device(self._raw(complianceState="noncompliant"))
        self.assertEqual(d.compliance_state, "noncompliant")

    def test_encrypted_flag(self):
        d = _parse_intune_device(self._raw(isEncrypted=True))
        self.assertTrue(d.encrypted)
        d2 = _parse_intune_device(self._raw(isEncrypted=False))
        self.assertFalse(d2.encrypted)


class TestJamfParsing(unittest.TestCase):
    # Jamf Pro API v1 (computers-preview) and v2 (mobile-devices) return flat dicts
    def _raw_computer(self, **kwargs):
        base = {
            "id": "42",
            "name": "Eve-MacBook",
            "serialNumber": "C02EVE",
            "model": "MacBook Pro (16-inch, 2023)",
            "username": "eve@company.com",
            "osVersion": "14.3.1",
            "filevault2Enabled": True,
        }
        base.update(kwargs)
        return base

    def _raw_mobile(self, **kwargs):
        base = {
            "id": "99",
            "name": "Frank's iPhone",
            "serialNumber": "SN789",
            "model": "iPhone 15 Pro",
            "username": "frank@company.com",
            "osVersion": "17.3",
        }
        base.update(kwargs)
        return base

    def test_computer_platform(self):
        d = _parse_jamf_computer(self._raw_computer())
        self.assertEqual(d.platform, "macos")
        self.assertEqual(d.provider, "jamf")

    def test_mobile_platform_iphone(self):
        d = _parse_jamf_mobile(self._raw_mobile())
        self.assertEqual(d.platform, "ios")

    def test_computer_email(self):
        d = _parse_jamf_computer(self._raw_computer())
        self.assertEqual(d.user_email, "eve@company.com")

    def test_mobile_email(self):
        d = _parse_jamf_mobile(self._raw_mobile())
        self.assertEqual(d.user_email, "frank@company.com")


# ── MDMBridgeManager — no-provider mode ──────────────────────────────────────

class TestMDMBridgeManagerNoProvider(unittest.TestCase):
    def setUp(self):
        import tempfile
        self._tmp = tempfile.mkdtemp()
        self._path = Path(self._tmp)

    def test_starts_without_provider(self):
        mgr = _mgr(self._path)
        asyncio.run(mgr.start())
        # No tasks started — no provider configured
        self.assertEqual(len(mgr._tasks), 0)

    def test_sync_returns_error_without_provider(self):
        mgr = _mgr(self._path)
        result = asyncio.run(mgr.sync())
        self.assertFalse(result["ok"])
        self.assertIn("error", result)

    def test_list_devices_empty(self):
        mgr = _mgr(self._path)
        self.assertEqual(mgr.list_devices(), [])

    def test_get_device_missing(self):
        mgr = _mgr(self._path)
        self.assertIsNone(mgr.get_device("nonexistent"))

    def test_is_enrolled_false(self):
        mgr = _mgr(self._path)
        self.assertFalse(mgr.is_enrolled("nobody@example.com"))

    def test_compliance_gaps_empty(self):
        mgr = _mgr(self._path)
        self.assertEqual(mgr.compliance_gaps(), [])

    def test_status(self):
        mgr = _mgr(self._path)
        s = mgr.status()
        self.assertIn("provider", s)
        self.assertIn("total_devices", s)
        # unconfigured → provider is empty string or "none"
        self.assertFalse(s["configured"])


# ── MDMBridgeManager — device management ─────────────────────────────────────

class TestMDMBridgeManagerDevices(unittest.TestCase):
    def setUp(self):
        import tempfile
        self._tmp = tempfile.mkdtemp()
        self._path = Path(self._tmp)
        self._mgr = _mgr(self._path)
        # Inject devices directly
        self._mgr._devices = {
            "dev-a": _make_device(id="dev-a", user_email="alice@x.com", platform="ios",
                                   compliance_state="compliant"),
            "dev-b": _make_device(id="dev-b", user_email="bob@x.com", platform="android",
                                   compliance_state="noncompliant", encrypted=False),
            "dev-c": _make_device(id="dev-c", user_email="alice@x.com", platform="macos",
                                   compliance_state="compliant", encrypted=True),
        }

    def test_list_all(self):
        self.assertEqual(len(self._mgr.list_devices()), 3)

    def test_filter_by_email(self):
        devs = self._mgr.list_devices(user_email="alice@x.com")
        self.assertEqual(len(devs), 2)
        for d in devs:
            self.assertEqual(d.user_email, "alice@x.com")

    def test_filter_email_case_insensitive(self):
        devs = self._mgr.list_devices(user_email="ALICE@X.COM")
        self.assertEqual(len(devs), 2)

    def test_filter_by_platform(self):
        devs = self._mgr.list_devices(platform="ios")
        self.assertEqual(len(devs), 1)
        self.assertEqual(devs[0].id, "dev-a")

    def test_filter_by_compliance(self):
        devs = self._mgr.list_devices(compliance_state="noncompliant")
        self.assertEqual(len(devs), 1)
        self.assertEqual(devs[0].id, "dev-b")

    def test_get_device_found(self):
        d = self._mgr.get_device("dev-b")
        self.assertIsNotNone(d)
        self.assertEqual(d.id, "dev-b")

    def test_get_device_not_found(self):
        self.assertIsNone(self._mgr.get_device("no-such"))

    def test_is_enrolled_true(self):
        self.assertTrue(self._mgr.is_enrolled("alice@x.com"))

    def test_is_enrolled_false(self):
        self.assertFalse(self._mgr.is_enrolled("nobody@x.com"))


# ── Compliance gaps ───────────────────────────────────────────────────────────

class TestComplianceGaps(unittest.TestCase):
    def setUp(self):
        import tempfile
        self._path = Path(tempfile.mkdtemp())
        self._mgr = _mgr(self._path)

    def _inject(self, **kwargs):
        d = _make_device(**kwargs)
        self._mgr._devices[d.id] = d

    def test_no_gaps_for_compliant_encrypted_device(self):
        self._inject(id="d1", compliance_state="compliant", encrypted=True)
        gaps = self._mgr.compliance_gaps()
        self.assertEqual(gaps, [])

    def test_noncompliant_gap(self):
        self._inject(id="d1", compliance_state="noncompliant", encrypted=True)
        gaps = self._mgr.compliance_gaps()
        types = [g["type"] for g in gaps]
        self.assertIn("noncompliant_device", types)

    def test_unencrypted_gap_for_windows(self):
        self._inject(id="d1", platform="windows", encrypted=False, compliance_state="compliant")
        gaps = self._mgr.compliance_gaps()
        types = [g["type"] for g in gaps]
        self.assertIn("unencrypted_device", types)

    def test_no_unencrypted_gap_for_ios(self):
        # iOS is always managed by the OS — no gap
        self._inject(id="d1", platform="ios", encrypted=False, compliance_state="compliant")
        gaps = self._mgr.compliance_gaps()
        types = [g["type"] for g in gaps]
        self.assertNotIn("unencrypted_device", types)

    def test_no_unencrypted_gap_for_chromeos(self):
        self._inject(id="d1", platform="chromeos", encrypted=False, compliance_state="compliant")
        gaps = self._mgr.compliance_gaps()
        types = [g["type"] for g in gaps]
        self.assertNotIn("unencrypted_device", types)

    def test_pending_removal_not_reported(self):
        self._inject(id="d1", compliance_state="noncompliant",
                     management_state="pending_removal")
        gaps = self._mgr.compliance_gaps()
        self.assertEqual(gaps, [])

    def test_vpn_ip_without_push_gap(self):
        self._inject(id="d1", compliance_state="compliant", encrypted=True,
                     vpn_ip="10.200.250.1", vpn_profile_pushed=False)
        gaps = self._mgr.compliance_gaps()
        types = [g["type"] for g in gaps]
        self.assertIn("vpn_profile_not_pushed", types)

    def test_no_vpn_gap_when_pushed(self):
        self._inject(id="d1", compliance_state="compliant", encrypted=True,
                     vpn_ip="10.200.250.1", vpn_profile_pushed=True)
        gaps = self._mgr.compliance_gaps()
        types = [g["type"] for g in gaps]
        self.assertNotIn("vpn_profile_not_pushed", types)


# ── Config get/set ────────────────────────────────────────────────────────────

class TestMDMBridgeManagerConfig(unittest.TestCase):
    def setUp(self):
        import tempfile
        self._path = Path(tempfile.mkdtemp())
        self._mgr = _mgr(self._path)

    def test_get_config_returns_current(self):
        cfg = self._mgr.get_config()
        self.assertIsInstance(cfg, MDMConfig)
        self.assertEqual(cfg.provider, "")

    def test_set_config_updates_and_saves(self):
        new_cfg = MDMConfig(provider="intune", intune_tenant_id="tid")
        self._mgr.set_config(new_cfg)
        self.assertEqual(self._mgr.get_config().provider, "intune")
        # Should have written to disk
        self.assertTrue(self._mgr._config_path.exists())

    def test_set_config_persisted(self):
        new_cfg = MDMConfig(provider="google", google_admin_email="admin@co.com")
        self._mgr.set_config(new_cfg)
        # Reload from disk
        mgr2 = _mgr(self._path)
        self.assertEqual(mgr2.get_config().provider, "google")
        self.assertEqual(mgr2.get_config().google_admin_email, "admin@co.com")


# ── Persistence ───────────────────────────────────────────────────────────────

class TestMDMBridgeManagerPersistence(unittest.TestCase):
    def setUp(self):
        import tempfile
        self._path = Path(tempfile.mkdtemp())

    def test_devices_persisted_and_reloaded(self):
        mgr = _mgr(self._path)
        d = _make_device(id="persist-dev", vpn_private_key="privkey", vpn_ip="10.200.250.7")
        mgr._devices[d.id] = d
        mgr._save_devices()
        # Reload
        mgr2 = _mgr(self._path)
        restored = mgr2.get_device("persist-dev")
        self.assertIsNotNone(restored)
        self.assertEqual(restored.vpn_ip, "10.200.250.7")
        self.assertEqual(restored.vpn_private_key, "privkey")

    def test_devices_file_mode_600(self):
        mgr = _mgr(self._path)
        d = _make_device(id="secure-dev", vpn_private_key="key")
        mgr._devices[d.id] = d
        mgr._save_devices()
        mode = oct(mgr._devices_path.stat().st_mode)[-3:]
        self.assertEqual(mode, "600")

    def test_ip_index_persisted(self):
        mgr = _mgr(self._path)
        mgr._next_mobile_ip_index = 15
        mgr._save_devices()
        mgr2 = _mgr(self._path)
        self.assertEqual(mgr2._next_mobile_ip_index, 15)

    def test_load_handles_corrupt_config(self):
        cfg_path = self._path / "mdm_bridge.json"
        cfg_path.parent.mkdir(parents=True, exist_ok=True)
        cfg_path.write_text("{invalid json")
        # Should not raise — just use defaults
        mgr = _mgr(self._path)
        self.assertEqual(mgr.get_config().provider, "")

    def test_load_handles_corrupt_devices(self):
        self._path.mkdir(parents=True, exist_ok=True)
        dev_path = self._path / "mdm_devices.json"
        dev_path.write_text("{invalid")
        mgr = _mgr(self._path)
        self.assertEqual(mgr.list_devices(), [])


# ── Sync — mock provider ──────────────────────────────────────────────────────

class TestMDMBridgeManagerSync(unittest.TestCase):
    def setUp(self):
        import tempfile
        self._path = Path(tempfile.mkdtemp())
        self._mgr = _mgr(self._path)
        self._mock_provider = AsyncMock()
        self._mgr._provider = self._mock_provider

    def _run(self, coro):
        return asyncio.run(coro)

    def test_sync_adds_new_devices(self):
        fresh = [_make_device(id="new-dev", user_email="alice@x.com")]
        self._mock_provider.list_devices = AsyncMock(return_value=fresh)
        result = self._run(self._mgr.sync())
        self.assertTrue(result["ok"])
        self.assertEqual(result["added"], 1)
        self.assertIsNotNone(self._mgr.get_device("new-dev"))

    def test_sync_updates_existing_preserving_vpn(self):
        # Pre-populate with VPN state
        existing = _make_device(id="dev-1", vpn_private_key="priv", vpn_public_key="pub",
                                 vpn_ip="10.200.250.2", vpn_profile_pushed=True)
        self._mgr._devices["dev-1"] = existing
        # Provider returns updated version (no VPN fields from provider)
        updated = _make_device(id="dev-1", os_version="15.0")
        self._mock_provider.list_devices = AsyncMock(return_value=[updated])
        self._run(self._mgr.sync())
        d = self._mgr.get_device("dev-1")
        self.assertEqual(d.os_version, "15.0")
        self.assertEqual(d.vpn_private_key, "priv")   # preserved
        self.assertEqual(d.vpn_ip, "10.200.250.2")    # preserved
        self.assertTrue(d.vpn_profile_pushed)          # preserved

    def test_sync_marks_missing_as_pending_removal(self):
        # Device in inventory but not returned by provider
        old = _make_device(id="gone-dev")
        self._mgr._devices["gone-dev"] = old
        self._mock_provider.list_devices = AsyncMock(return_value=[])
        self._run(self._mgr.sync())
        d = self._mgr.get_device("gone-dev")
        self.assertEqual(d.management_state, "pending_removal")

    def test_sync_fires_event(self):
        events = []
        q: asyncio.Queue = asyncio.Queue()
        self._mgr._event_queue = q
        self._mock_provider.list_devices = AsyncMock(return_value=[])
        self._run(self._mgr.sync())
        while not q.empty():
            events.append(q.get_nowait())
        types = [e["type"] for e in events]
        self.assertIn("mdm.sync.complete", types)

    def test_sync_handles_provider_error(self):
        self._mock_provider.list_devices = AsyncMock(side_effect=RuntimeError("Network error"))
        result = self._run(self._mgr.sync())
        self.assertFalse(result["ok"])
        self.assertIn("Network error", result["error"])


# ── VPN profile push ──────────────────────────────────────────────────────────

class TestVPNProfilePush(unittest.TestCase):
    def setUp(self):
        import tempfile
        self._path = Path(tempfile.mkdtemp())
        cfg = MDMConfig(
            provider="intune",
            wg_endpoint="vpn.company.com:51821",
            wg_server_public_key="SERVERPUB=",
            wg_dns="10.200.1.1",
        )
        self._mgr = _mgr(self._path, config=cfg)
        self._mock_provider = AsyncMock()
        self._mock_provider.push_custom_profile = AsyncMock(return_value=True)
        self._mgr._provider = self._mock_provider

    def _run(self, coro):
        return asyncio.run(coro)

    def test_push_generates_keypair(self):
        dev = _make_device(id="phone-1", platform="android")
        self._mgr._devices["phone-1"] = dev
        self._run(self._mgr.push_vpn_profile("phone-1"))
        d = self._mgr.get_device("phone-1")
        self.assertNotEqual(d.vpn_private_key, "")
        self.assertNotEqual(d.vpn_public_key, "")

    def test_push_assigns_ip_in_mobile_subnet(self):
        dev = _make_device(id="phone-1", platform="android")
        self._mgr._devices["phone-1"] = dev
        self._run(self._mgr.push_vpn_profile("phone-1"))
        d = self._mgr.get_device("phone-1")
        self.assertTrue(d.vpn_ip.startswith(MOBILE_WG_SUBNET + "."))

    def test_push_marks_profile_pushed(self):
        dev = _make_device(id="phone-1", platform="android")
        self._mgr._devices["phone-1"] = dev
        self._run(self._mgr.push_vpn_profile("phone-1"))
        d = self._mgr.get_device("phone-1")
        self.assertTrue(d.vpn_profile_pushed)

    def test_ios_gets_mobileconfig(self):
        dev = _make_device(id="iphone-1", platform="ios")
        self._mgr._devices["iphone-1"] = dev
        self._run(self._mgr.push_vpn_profile("iphone-1"))
        call_args = self._mock_provider.push_custom_profile.call_args
        profile = call_args[0][2]  # 3rd positional arg
        self.assertIn("<?xml", profile)

    def test_android_gets_wg_text_config(self):
        dev = _make_device(id="android-1", platform="android")
        self._mgr._devices["android-1"] = dev
        self._run(self._mgr.push_vpn_profile("android-1"))
        call_args = self._mock_provider.push_custom_profile.call_args
        profile = call_args[0][2]
        self.assertIn("[Interface]", profile)

    def test_windows_gets_wg_text_config(self):
        dev = _make_device(id="win-1", platform="windows")
        self._mgr._devices["win-1"] = dev
        self._run(self._mgr.push_vpn_profile("win-1"))
        call_args = self._mock_provider.push_custom_profile.call_args
        profile = call_args[0][2]
        self.assertIn("[Interface]", profile)

    def test_keypair_reused_on_second_push(self):
        dev = _make_device(id="phone-1", platform="android",
                            vpn_private_key="existing-priv",
                            vpn_public_key="existing-pub",
                            vpn_ip="10.200.250.5")
        self._mgr._devices["phone-1"] = dev
        self._run(self._mgr.push_vpn_profile("phone-1"))
        d = self._mgr.get_device("phone-1")
        self.assertEqual(d.vpn_private_key, "existing-priv")
        self.assertEqual(d.vpn_ip, "10.200.250.5")

    def test_push_fires_event(self):
        q: asyncio.Queue = asyncio.Queue()
        self._mgr._event_queue = q
        dev = _make_device(id="phone-1", platform="android")
        self._mgr._devices["phone-1"] = dev
        self._run(self._mgr.push_vpn_profile("phone-1"))
        events = []
        while not q.empty():
            events.append(q.get_nowait())
        types = [e["type"] for e in events]
        self.assertIn("mdm.vpn.profile_pushed", types)

    def test_push_raises_for_unknown_device(self):
        with self.assertRaises(ValueError):
            self._run(self._mgr.push_vpn_profile("no-such-device"))

    def test_push_raises_without_wg_endpoint(self):
        cfg = MDMConfig(provider="intune", wg_server_public_key="PUB=")  # no endpoint
        mgr = _mgr(self._path / "sub", config=cfg)
        mgr._provider = self._mock_provider
        dev = _make_device(id="phone-2", platform="android")
        mgr._devices["phone-2"] = dev
        with self.assertRaises(RuntimeError):
            self._run(mgr.push_vpn_profile("phone-2"))

    def test_ip_index_increments(self):
        devs = [_make_device(id=f"p{i}", platform="android") for i in range(3)]
        for d in devs:
            self._mgr._devices[d.id] = d
        for d in devs:
            self._run(self._mgr.push_vpn_profile(d.id))
        ips = {self._mgr.get_device(d.id).vpn_ip for d in devs}
        # All IPs should be distinct
        self.assertEqual(len(ips), 3)


# ── Offboarding ───────────────────────────────────────────────────────────────

class TestOffboarding(unittest.TestCase):
    def setUp(self):
        import tempfile
        self._path = Path(tempfile.mkdtemp())
        self._mgr = _mgr(self._path)
        self._mock_provider = AsyncMock()
        self._mock_provider.unenroll = AsyncMock(return_value=True)
        self._mock_provider.remote_wipe = AsyncMock(return_value=True)
        self._mock_provider.remote_lock = AsyncMock(return_value=True)
        self._mgr._provider = self._mock_provider
        # Two devices for alice
        for i in (1, 2):
            d = _make_device(id=f"alice-dev-{i}", user_email="alice@x.com")
            self._mgr._devices[d.id] = d
        # One device for bob
        b = _make_device(id="bob-dev-1", user_email="bob@x.com")
        self._mgr._devices[b.id] = b

    def _run(self, coro):
        return asyncio.run(coro)

    def test_offboard_unenrolls_all_user_devices(self):
        result = self._run(self._mgr.offboard_user("alice@x.com", wipe=False))
        self.assertEqual(self._mock_provider.unenroll.call_count, 2)
        self.assertIn("results", result)

    def test_offboard_wipe_calls_remote_wipe(self):
        self._run(self._mgr.offboard_user("alice@x.com", wipe=True))
        self.assertEqual(self._mock_provider.remote_wipe.call_count, 2)
        self.assertEqual(self._mock_provider.unenroll.call_count, 0)

    def test_offboard_does_not_affect_other_users(self):
        self._run(self._mgr.offboard_user("alice@x.com", wipe=False))
        # Bob's device untouched
        calls = [str(c) for c in self._mock_provider.unenroll.call_args_list]
        self.assertFalse(any("bob-dev" in c for c in calls))

    def test_offboard_raises_without_provider(self):
        mgr = _mgr(self._path / "sub2")
        with self.assertRaises(RuntimeError):
            self._run(mgr.offboard_user("alice@x.com"))


if __name__ == "__main__":
    unittest.main()
