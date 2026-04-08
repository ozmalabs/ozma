# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for controller/router_mode.py."""

from __future__ import annotations

import asyncio
import json
import sys
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

sys.path.insert(0, str(Path(__file__).parents[3] / "controller"))

from router_mode import (
    RouterConfig,
    RouterModeManager,
    NFT_TABLE_NAME,
    _build_nft_ruleset,
    _build_dnsmasq_conf,
)


def run(coro):
    return asyncio.run(coro)


# ---------------------------------------------------------------------------
# RouterConfig
# ---------------------------------------------------------------------------

class TestRouterConfig:
    def test_defaults(self):
        cfg = RouterConfig()
        assert cfg.enabled is False
        assert cfg.wan_interface == "eth0"
        assert cfg.lan_interface == "eth1"
        assert cfg.iot_vlan_id == 20
        assert cfg.lan_gateway == "192.168.1.1"
        assert cfg.iot_gateway == "192.168.20.1"
        assert cfg.upstream_dns == ["1.1.1.1", "1.0.0.1"]
        assert cfg.trusted_camera_ips == []
        assert cfg.cloud_allow_rules == []

    def test_to_dict_roundtrip(self):
        cfg = RouterConfig(
            enabled=True,
            wan_interface="ens3",
            lan_interface="ens4",
            iot_vlan_id=30,
            lan_gateway="10.0.0.1",
            upstream_dns=["8.8.8.8"],
        )
        d = cfg.to_dict()
        restored = RouterConfig.from_dict(d)
        assert restored.enabled is True
        assert restored.wan_interface == "ens3"
        assert restored.iot_vlan_id == 30
        assert restored.upstream_dns == ["8.8.8.8"]

    def test_from_dict_defaults(self):
        cfg = RouterConfig.from_dict({})
        assert cfg.enabled is False
        assert cfg.iot_vlan_id == 20


# ---------------------------------------------------------------------------
# nftables ruleset generation
# ---------------------------------------------------------------------------

class TestNFTRuleset:
    def test_table_name(self):
        cfg = RouterConfig()
        nft = _build_nft_ruleset(cfg)
        assert NFT_TABLE_NAME in nft

    def test_wan_masquerade(self):
        cfg = RouterConfig(wan_interface="ens3")
        nft = _build_nft_ruleset(cfg)
        assert "ens3" in nft
        assert "masquerade" in nft

    def test_iot_vlan_interface_name(self):
        cfg = RouterConfig(lan_interface="eth1", iot_vlan_id=20)
        nft = _build_nft_ruleset(cfg)
        assert "eth1.20" in nft

    def test_controller_api_ports_allowed(self):
        cfg = RouterConfig()
        nft = _build_nft_ruleset(cfg)
        assert "7380" in nft
        assert "5000" in nft

    def test_iot_to_wan_denied(self):
        cfg = RouterConfig()
        nft = _build_nft_ruleset(cfg)
        assert "drop" in nft

    def test_cloud_allow_rule_included(self):
        cfg = RouterConfig()
        cfg.cloud_allow_rules = [
            {"ip": "192.168.20.101", "destination": "52.94.76.0/22", "comment": "Alexa cloud"},
        ]
        nft = _build_nft_ruleset(cfg)
        assert "192.168.20.101" in nft
        assert "52.94.76.0/22" in nft
        assert "Alexa cloud" in nft

    def test_trusted_camera_ip_included(self):
        cfg = RouterConfig()
        cfg.trusted_camera_ips = ["192.168.1.50"]
        nft = _build_nft_ruleset(cfg)
        assert "192.168.1.50" in nft

    def test_multiple_cloud_rules(self):
        cfg = RouterConfig()
        cfg.cloud_allow_rules = [
            {"ip": "192.168.20.100", "destination": "1.2.3.4", "comment": "R1"},
            {"ip": "192.168.20.101", "destination": "5.6.7.8", "comment": "R2"},
        ]
        nft = _build_nft_ruleset(cfg)
        assert "1.2.3.4" in nft
        assert "5.6.7.8" in nft

    def test_default_policy_drop(self):
        cfg = RouterConfig()
        nft = _build_nft_ruleset(cfg)
        assert "policy drop" in nft


# ---------------------------------------------------------------------------
# dnsmasq config generation
# ---------------------------------------------------------------------------

class TestDnsmasqConf:
    def test_interfaces_present(self):
        cfg = RouterConfig(lan_interface="eth1", iot_vlan_id=20)
        conf = _build_dnsmasq_conf(cfg)
        assert "interface=eth1" in conf
        assert "interface=eth1.20" in conf

    def test_dhcp_ranges(self):
        cfg = RouterConfig(
            lan_dhcp_start="192.168.1.100",
            lan_dhcp_end="192.168.1.200",
            iot_dhcp_start="192.168.20.100",
            iot_dhcp_end="192.168.20.200",
        )
        conf = _build_dnsmasq_conf(cfg)
        assert "192.168.1.100" in conf
        assert "192.168.1.200" in conf
        assert "192.168.20.100" in conf
        assert "192.168.20.200" in conf

    def test_upstream_dns(self):
        cfg = RouterConfig(upstream_dns=["9.9.9.9", "149.112.112.112"])
        conf = _build_dnsmasq_conf(cfg)
        assert "9.9.9.9" in conf

    def test_no_resolv(self):
        cfg = RouterConfig()
        conf = _build_dnsmasq_conf(cfg)
        assert "no-resolv" in conf


# ---------------------------------------------------------------------------
# Persistence
# ---------------------------------------------------------------------------

class TestRouterModePersistence:
    def test_save_and_load(self, tmp_path):
        state = tmp_path / "router_mode_state.json"
        mgr = RouterModeManager(state_path=state)
        mgr._config.wan_interface = "ens3"
        mgr._config.enabled = True
        mgr._save()

        mgr2 = RouterModeManager(state_path=state)
        assert mgr2._config.wan_interface == "ens3"
        assert mgr2._config.enabled is True

    def test_file_permissions(self, tmp_path):
        state = tmp_path / "router_mode_state.json"
        mgr = RouterModeManager(state_path=state)
        mgr._save()
        assert oct(state.stat().st_mode)[-3:] == "600"

    def test_load_missing(self, tmp_path):
        state = tmp_path / "router_mode_state.json"
        mgr = RouterModeManager(state_path=state)
        assert mgr._config.enabled is False


# ---------------------------------------------------------------------------
# Cloud allow rules
# ---------------------------------------------------------------------------

class TestCloudAllowRules:
    def test_add_rule(self, tmp_path):
        state = tmp_path / "state.json"
        mgr = RouterModeManager(state_path=state)
        mgr._active = False  # Don't apply nftables in tests

        rule = run(mgr.add_cloud_allow_rule("192.168.20.50", "52.1.2.3", "My device"))
        assert rule["ip"] == "192.168.20.50"
        assert rule["destination"] == "52.1.2.3"
        assert rule["comment"] == "My device"
        assert "added_at" in rule

    def test_add_rule_persisted(self, tmp_path):
        state = tmp_path / "state.json"
        mgr = RouterModeManager(state_path=state)
        mgr._active = False
        run(mgr.add_cloud_allow_rule("192.168.20.51", "1.2.3.4", ""))

        mgr2 = RouterModeManager(state_path=state)
        assert len(mgr2._config.cloud_allow_rules) == 1

    def test_remove_rule(self, tmp_path):
        state = tmp_path / "state.json"
        mgr = RouterModeManager(state_path=state)
        mgr._active = False
        run(mgr.add_cloud_allow_rule("10.0.0.1", "5.5.5.5", ""))
        removed = run(mgr.remove_cloud_allow_rule("10.0.0.1", "5.5.5.5"))
        assert removed is True
        assert len(mgr._config.cloud_allow_rules) == 0

    def test_remove_nonexistent_rule(self, tmp_path):
        state = tmp_path / "state.json"
        mgr = RouterModeManager(state_path=state)
        mgr._active = False
        removed = run(mgr.remove_cloud_allow_rule("10.0.0.1", "9.9.9.9"))
        assert removed is False

    def test_list_rules(self, tmp_path):
        state = tmp_path / "state.json"
        mgr = RouterModeManager(state_path=state)
        mgr._active = False
        run(mgr.add_cloud_allow_rule("1.1.1.1", "2.2.2.2", "R1"))
        run(mgr.add_cloud_allow_rule("3.3.3.3", "4.4.4.4", "R2"))
        rules = mgr.list_cloud_allow_rules()
        assert len(rules) == 2

    def test_audit_timestamp(self, tmp_path):
        """Each cloud rule must include an added_at timestamp."""
        state = tmp_path / "state.json"
        mgr = RouterModeManager(state_path=state)
        mgr._active = False
        rule = run(mgr.add_cloud_allow_rule("1.1.1.1", "2.2.2.2", ""))
        assert isinstance(rule.get("added_at"), float)
        assert rule["added_at"] > 0


# ---------------------------------------------------------------------------
# Camera VLAN exemption
# ---------------------------------------------------------------------------

class TestCameraVLANExemption:
    def test_add_trusted_camera(self, tmp_path):
        state = tmp_path / "state.json"
        mgr = RouterModeManager(state_path=state)
        mgr._active = False
        run(mgr.add_trusted_camera("192.168.1.50"))
        assert "192.168.1.50" in mgr._config.trusted_camera_ips

    def test_add_same_camera_twice_idempotent(self, tmp_path):
        state = tmp_path / "state.json"
        mgr = RouterModeManager(state_path=state)
        mgr._active = False
        run(mgr.add_trusted_camera("192.168.1.50"))
        run(mgr.add_trusted_camera("192.168.1.50"))
        assert mgr._config.trusted_camera_ips.count("192.168.1.50") == 1

    def test_remove_trusted_camera(self, tmp_path):
        state = tmp_path / "state.json"
        mgr = RouterModeManager(state_path=state)
        mgr._active = False
        run(mgr.add_trusted_camera("192.168.1.60"))
        run(mgr.remove_trusted_camera("192.168.1.60"))
        assert "192.168.1.60" not in mgr._config.trusted_camera_ips

    def test_remove_nonexistent_camera_noop(self, tmp_path):
        state = tmp_path / "state.json"
        mgr = RouterModeManager(state_path=state)
        mgr._active = False
        run(mgr.remove_trusted_camera("10.0.0.99"))  # should not raise

    def test_trusted_camera_in_nftables(self, tmp_path):
        """Trusted camera IP appears in generated nftables ruleset."""
        state = tmp_path / "state.json"
        mgr = RouterModeManager(state_path=state)
        mgr._config.trusted_camera_ips = ["192.168.1.50"]
        nft = _build_nft_ruleset(mgr._config)
        assert "192.168.1.50" in nft

    def test_camera_ips_persisted(self, tmp_path):
        state = tmp_path / "state.json"
        mgr = RouterModeManager(state_path=state)
        mgr._active = False
        run(mgr.add_trusted_camera("10.0.1.2"))
        mgr2 = RouterModeManager(state_path=state)
        assert "10.0.1.2" in mgr2._config.trusted_camera_ips


# ---------------------------------------------------------------------------
# Status
# ---------------------------------------------------------------------------

class TestRouterModeStatus:
    def test_status_keys(self, tmp_path):
        state = tmp_path / "state.json"
        mgr = RouterModeManager(state_path=state)
        status = mgr.get_status()
        for key in ("enabled", "active", "wan_interface", "lan_interface",
                    "iot_vlan_id", "lan_gateway", "iot_gateway",
                    "trusted_camera_count", "cloud_allow_rules"):
            assert key in status, f"Missing key: {key}"

    def test_not_active_initially(self, tmp_path):
        state = tmp_path / "state.json"
        mgr = RouterModeManager(state_path=state)
        assert mgr.get_status()["active"] is False


# ---------------------------------------------------------------------------
# Frigate hardware detection
# ---------------------------------------------------------------------------

class TestFrigateAutoStart:
    def test_detector_flag_nvidia(self):
        assert RouterModeManager._detector_flag("nvidia") == "cuda"

    def test_detector_flag_intel(self):
        assert RouterModeManager._detector_flag("intel") == "openvino"

    def test_detector_flag_rknpu2(self):
        assert RouterModeManager._detector_flag("rknpu2") == "rknpu2"

    def test_detector_flag_hailo(self):
        assert RouterModeManager._detector_flag("hailo") == "hailo8l"

    def test_detector_flag_amd(self):
        assert RouterModeManager._detector_flag("amd") == "rocm"

    def test_detector_flag_unknown(self):
        assert RouterModeManager._detector_flag("unknown") == "cpu"

    def test_get_ram_gb(self):
        ram = RouterModeManager._get_ram_gb()
        assert isinstance(ram, float)
        assert ram >= 0

    def test_frigate_skipped_no_docker(self, tmp_path):
        """When Docker is unavailable, Frigate auto-start returns False."""
        state = tmp_path / "state.json"
        mgr = RouterModeManager(state_path=state)

        async def fake_docker() -> bool:
            return False

        with patch.object(RouterModeManager, "_docker_available", staticmethod(fake_docker)):
            result = run(mgr.start_frigate_if_capable())
        assert result is False
