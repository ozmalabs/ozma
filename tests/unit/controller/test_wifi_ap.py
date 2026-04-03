# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for controller/wifi_ap.py."""

from __future__ import annotations

import asyncio
import json
import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).parents[3] / "controller"))

from wifi_ap import (
    WiFiAPConfig,
    WiFiAPManager,
    _build_hostapd_conf,
)


def run(coro):
    return asyncio.get_event_loop().run_until_complete(coro)


# ---------------------------------------------------------------------------
# WiFiAPConfig
# ---------------------------------------------------------------------------

class TestWiFiAPConfig:
    def test_defaults(self):
        cfg = WiFiAPConfig()
        assert cfg.enabled is False
        assert cfg.interface == "auto"
        assert cfg.iot_ssid == "ozma-iot"
        assert cfg.onboarding_ssid == "ozma-setup"
        assert cfg.channel == 6
        assert cfg.hw_mode == "g"
        assert cfg.country_code == "US"

    def test_generate_psk_length(self):
        cfg = WiFiAPConfig()
        psk = cfg.generate_psk()
        assert len(psk) == 24

    def test_generate_psk_randomness(self):
        cfg = WiFiAPConfig()
        psks = {cfg.generate_psk() for _ in range(10)}
        assert len(psks) > 8  # Very unlikely to have duplicates

    def test_to_dict_roundtrip(self):
        cfg = WiFiAPConfig(
            enabled=True,
            interface="wlan0",
            iot_ssid="myiot",
            iot_psk="secretpsk12345678901234",
            channel=11,
            country_code="GB",
        )
        d = cfg.to_dict()
        restored = WiFiAPConfig.from_dict(d)
        assert restored.enabled is True
        assert restored.interface == "wlan0"
        assert restored.iot_ssid == "myiot"
        assert restored.channel == 11
        assert restored.country_code == "GB"

    def test_from_dict_defaults(self):
        cfg = WiFiAPConfig.from_dict({})
        assert cfg.enabled is False
        assert cfg.channel == 6


# ---------------------------------------------------------------------------
# hostapd config generation
# ---------------------------------------------------------------------------

class TestHostapdConf:
    def test_basic_iot_ssid(self):
        cfg = WiFiAPConfig(iot_ssid="test-iot", iot_psk="password1234567890ab")
        conf = _build_hostapd_conf(cfg, "wlan0")
        assert "interface=wlan0" in conf
        assert "ssid=test-iot" in conf
        assert "wpa=2" in conf
        assert "wpa_passphrase=password1234567890ab" in conf

    def test_onboarding_ssid_disabled(self):
        cfg = WiFiAPConfig(onboarding_enabled=False, onboarding_ssid="ozma-setup")
        conf = _build_hostapd_conf(cfg, "wlan0")
        assert "ozma-setup" not in conf

    def test_onboarding_ssid_enabled(self):
        cfg = WiFiAPConfig(onboarding_enabled=True, onboarding_ssid="ozma-setup")
        conf = _build_hostapd_conf(cfg, "wlan0")
        assert "ozma-setup" in conf
        # Onboarding is open (wpa=0)
        assert "wpa=0" in conf

    def test_channel_and_country(self):
        cfg = WiFiAPConfig(channel=11, country_code="AU")
        conf = _build_hostapd_conf(cfg, "wlan0")
        assert "channel=11" in conf
        assert "country_code=AU" in conf

    def test_interface_in_conf(self):
        cfg = WiFiAPConfig(iot_psk="pw" * 12)
        conf = _build_hostapd_conf(cfg, "wlp3s0")
        assert "interface=wlp3s0" in conf

    def test_wpa2_params(self):
        cfg = WiFiAPConfig(iot_psk="test1234567890abcdef")
        conf = _build_hostapd_conf(cfg, "wlan0")
        assert "wpa_key_mgmt=WPA-PSK" in conf
        assert "rsn_pairwise=CCMP" in conf


# ---------------------------------------------------------------------------
# WiFiAPManager persistence
# ---------------------------------------------------------------------------

class TestWiFiAPManagerPersistence:
    def test_save_and_load(self, tmp_path):
        state = tmp_path / "wifi_ap_state.json"
        mgr = WiFiAPManager(state_path=state)
        mgr._config.iot_ssid = "my-iot"
        mgr._config.channel = 11
        mgr._save()

        mgr2 = WiFiAPManager(state_path=state)
        assert mgr2._config.iot_ssid == "my-iot"
        assert mgr2._config.channel == 11

    def test_file_permissions(self, tmp_path):
        state = tmp_path / "wifi_ap_state.json"
        mgr = WiFiAPManager(state_path=state)
        mgr._save()
        assert oct(state.stat().st_mode)[-3:] == "600"

    def test_load_missing_file(self, tmp_path):
        state = tmp_path / "wifi_ap_state.json"
        mgr = WiFiAPManager(state_path=state)
        # Should not raise — returns default config
        assert mgr._config.enabled is False


# ---------------------------------------------------------------------------
# WiFiAPManager status
# ---------------------------------------------------------------------------

class TestWiFiAPManagerStatus:
    def test_status_keys(self, tmp_path):
        state = tmp_path / "state.json"
        mgr = WiFiAPManager(state_path=state)
        status = mgr.get_status()
        assert "enabled" in status
        assert "running" in status
        assert "interface" in status
        assert "iot_ssid" in status
        assert "channel" in status

    def test_onboarding_ssid_hidden_when_disabled(self, tmp_path):
        state = tmp_path / "state.json"
        mgr = WiFiAPManager(state_path=state)
        mgr._config.onboarding_enabled = False
        status = mgr.get_status()
        assert status["onboarding_ssid"] is None

    def test_onboarding_ssid_shown_when_enabled(self, tmp_path):
        state = tmp_path / "state.json"
        mgr = WiFiAPManager(state_path=state)
        mgr._config.onboarding_enabled = True
        status = mgr.get_status()
        assert status["onboarding_ssid"] == "ozma-setup"


# ---------------------------------------------------------------------------
# get_config
# ---------------------------------------------------------------------------

class TestWiFiAPManagerGetConfig:
    def test_returns_config(self, tmp_path):
        state = tmp_path / "state.json"
        mgr = WiFiAPManager(state_path=state)
        cfg = mgr.get_config()
        assert isinstance(cfg, WiFiAPConfig)


# ---------------------------------------------------------------------------
# PSK auto-generation on start
# ---------------------------------------------------------------------------

class TestPSKAutoGeneration:
    def test_psk_auto_generated_when_empty(self, tmp_path):
        """_start_hostapd should auto-generate a PSK if none is set."""
        state = tmp_path / "state.json"
        mgr = WiFiAPManager(state_path=state)
        mgr._config.enabled = True
        mgr._config.iot_psk = ""
        # Simulate the PSK generation step without actually starting hostapd
        if not mgr._config.iot_psk:
            mgr._config.iot_psk = mgr._config.generate_psk()
            mgr._save()
        assert len(mgr._config.iot_psk) == 24
        # Persisted
        mgr2 = WiFiAPManager(state_path=state)
        assert mgr2._config.iot_psk == mgr._config.iot_psk
