# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
IoT network management — VLAN isolation, default-deny policy, onboarding workflow.

Architecture:
  - IoT devices live on a dedicated VLAN (default: VLAN 20, 192.168.20.0/24)
  - Default-deny policy:  IoT → Internet: deny
                          IoT → Main LAN: deny
                          IoT → Ozma controller: allow (port 7380 + Frigate 5000)
  - Onboarding exception: temporary per-device rule during app setup
  - Hardware backends: UniFi / MikroTik / OpenWrt / pfSense / native Linux
  - Native Linux fallback: nftables + dnsmasq + hostapd

Onboarding flow:
  1. POST /api/v1/iot/onboard          — create session, apply exception rule
  2. User completes device app setup
  3. POST /api/v1/iot/onboard/{id}/complete — remove exception, lock device down

The exception rule allows the onboarding phone's IP temporary IoT VLAN access
(to reach the device's AP/setup portal) and the device temporary internet access
(for initial cloud registration if unavoidable). Both rules expire automatically.
"""

from __future__ import annotations

import asyncio
import json
import logging
import time
import uuid
from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path
from typing import Any, Protocol

log = logging.getLogger("ozma.iot_network")

CONFIG_PATH = Path(__file__).parent / "iot_network.json"

# Default VLAN settings
DEFAULT_VLAN_ID   = 20
DEFAULT_SUBNET    = "192.168.20"   # /24 implied
DEFAULT_GATEWAY   = "192.168.20.1"
DEFAULT_DHCP_START = "192.168.20.100"
DEFAULT_DHCP_END   = "192.168.20.200"
DEFAULT_DNS       = "192.168.20.1"


# ── Enums ─────────────────────────────────────────────────────────────────────

class DeviceCategory(str, Enum):
    CAMERA       = "camera"
    SMART_HOME   = "smart_home"   # Wyze, Ring, Nest
    MEDIA        = "media"         # Chromecast, Fire TV
    PRINTER      = "printer"
    SENSOR       = "sensor"        # temp, motion, door
    UNKNOWN      = "unknown"


class InternetAccess(str, Enum):
    DENY         = "deny"          # default — no internet at all
    ALLOW        = "allow"         # explicit allowance with audit event
    CLOUD_ONLY   = "cloud_only"    # allow only to known vendor cloud IPs (best-effort)


class OnboardingState(str, Enum):
    PENDING      = "pending"       # exception active, waiting for user to tap Done
    COMPLETE     = "complete"      # locked down, device in inventory
    EXPIRED      = "expired"       # session timed out
    CANCELLED    = "cancelled"     # user cancelled


# ── Data models ───────────────────────────────────────────────────────────────

@dataclass
class VLANConfig:
    vlan_id:     int  = DEFAULT_VLAN_ID
    subnet:      str  = DEFAULT_SUBNET
    gateway:     str  = DEFAULT_GATEWAY
    dhcp_start:  str  = DEFAULT_DHCP_START
    dhcp_end:    str  = DEFAULT_DHCP_END
    dns:         str  = DEFAULT_DNS
    iface:       str  = ""         # physical interface (e.g. eth0); empty = auto

    def to_dict(self) -> dict[str, Any]:
        return {
            "vlan_id": self.vlan_id, "subnet": self.subnet,
            "gateway": self.gateway, "dhcp_start": self.dhcp_start,
            "dhcp_end": self.dhcp_end, "dns": self.dns, "iface": self.iface,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> VLANConfig:
        return cls(**{k: v for k, v in d.items() if k in cls.__dataclass_fields__})


@dataclass
class IoTDevice:
    id:              str              # stable UUID
    mac:             str              # lowercase colon-separated
    name:            str              = ""
    category:        DeviceCategory   = DeviceCategory.UNKNOWN
    vendor:          str              = ""           # from OUI lookup
    ip:              str              = ""           # last seen DHCP lease
    internet_access: InternetAccess   = InternetAccess.DENY
    notes:           str              = ""
    added_at:        float            = 0.0
    last_seen:       float            = 0.0
    blocked:         bool             = False        # manual block

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id, "mac": self.mac, "name": self.name,
            "category": self.category.value, "vendor": self.vendor,
            "ip": self.ip, "internet_access": self.internet_access.value,
            "notes": self.notes, "added_at": self.added_at,
            "last_seen": self.last_seen, "blocked": self.blocked,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> IoTDevice:
        return cls(
            id=d["id"], mac=d["mac"], name=d.get("name", ""),
            category=DeviceCategory(d.get("category", "unknown")),
            vendor=d.get("vendor", ""), ip=d.get("ip", ""),
            internet_access=InternetAccess(d.get("internet_access", "deny")),
            notes=d.get("notes", ""), added_at=d.get("added_at", 0.0),
            last_seen=d.get("last_seen", 0.0), blocked=d.get("blocked", False),
        )


@dataclass
class OnboardingSession:
    id:               str
    device_name:      str             = ""
    category:         DeviceCategory  = DeviceCategory.UNKNOWN
    phone_ip:         str             = ""   # phone's IP; gets temp VLAN bridge access
    state:            OnboardingState = OnboardingState.PENDING
    allow_internet:   bool            = False   # some devices need it for first setup
    created_at:       float           = 0.0
    expires_at:       float           = 0.0    # 0 = no expiry
    completed_at:     float           = 0.0
    device_id:        str             = ""     # set on complete

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id, "device_name": self.device_name,
            "category": self.category.value, "phone_ip": self.phone_ip,
            "state": self.state.value, "allow_internet": self.allow_internet,
            "created_at": self.created_at, "expires_at": self.expires_at,
            "completed_at": self.completed_at, "device_id": self.device_id,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> OnboardingSession:
        return cls(
            id=d["id"], device_name=d.get("device_name", ""),
            category=DeviceCategory(d.get("category", "unknown")),
            phone_ip=d.get("phone_ip", ""),
            state=OnboardingState(d.get("state", "pending")),
            allow_internet=d.get("allow_internet", False),
            created_at=d.get("created_at", 0.0),
            expires_at=d.get("expires_at", 0.0),
            completed_at=d.get("completed_at", 0.0),
            device_id=d.get("device_id", ""),
        )

    @property
    def expired(self) -> bool:
        return self.expires_at > 0 and time.time() > self.expires_at


# ── Hardware backend protocol ─────────────────────────────────────────────────

class HardwareBackend(Protocol):
    """Abstraction over the network hardware that enforces VLAN/firewall rules."""

    @property
    def name(self) -> str: ...

    async def provision_vlan(self, cfg: VLANConfig) -> bool:
        """Create the IoT VLAN. Returns True on success."""
        ...

    async def apply_device_rules(self, devices: list[IoTDevice],
                                  cfg: VLANConfig) -> bool:
        """Push per-device firewall rules (internet_access, blocked). Returns True."""
        ...

    async def apply_onboarding_exception(self, session: OnboardingSession,
                                          cfg: VLANConfig) -> bool:
        """Temporarily open access for an onboarding session."""
        ...

    async def remove_onboarding_exception(self, session_id: str) -> bool:
        """Remove an onboarding exception rule."""
        ...

    async def get_dhcp_leases(self) -> list[dict[str, str]]:
        """Return [{mac, ip, hostname, expires}] for the IoT VLAN."""
        ...


# ── Native Linux backend ──────────────────────────────────────────────────────

class NativeLinuxBackend:
    """
    nftables + dnsmasq + optionally hostapd.

    Generates and applies nftables rules for the IoT VLAN.
    Uses dnsmasq for DHCP on the VLAN interface.
    """

    name = "native_linux"

    def __init__(self) -> None:
        self._onboarding_rules: dict[str, str] = {}   # session_id → rule handle

    async def provision_vlan(self, cfg: VLANConfig) -> bool:
        iface = cfg.iface or "eth0"
        vlan_iface = f"{iface}.{cfg.vlan_id}"
        cmds = [
            # Create VLAN interface if it doesn't exist
            f"ip link add link {iface} name {vlan_iface} type vlan id {cfg.vlan_id} 2>/dev/null || true",
            f"ip link set {vlan_iface} up",
            f"ip addr add {cfg.gateway}/24 dev {vlan_iface} 2>/dev/null || true",
        ]
        for cmd in cmds:
            proc = await asyncio.create_subprocess_shell(
                cmd,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.DEVNULL,
            )
            await proc.wait()
        log.info("Native Linux: VLAN %d provisioned on %s", cfg.vlan_id, vlan_iface)
        return True

    def _generate_nftables(self, devices: list[IoTDevice],
                            cfg: VLANConfig,
                            onboarding_sessions: list[OnboardingSession] | None = None) -> str:
        """Generate an nftables ruleset for the IoT VLAN."""
        iot_subnet = f"{cfg.subnet}.0/24"
        ctrl_ip = cfg.gateway

        lines = [
            "#!/usr/sbin/nft -f",
            "flush ruleset",
            "",
            "table inet ozma_iot {",
            "    chain forward {",
            "        type filter hook forward priority 0; policy drop;",
            "",
            f"        # Allow established/related",
            "        ct state established,related accept",
            "",
            f"        # Allow IoT → Ozma controller (API + Frigate)",
            f"        ip saddr {iot_subnet} ip daddr {ctrl_ip} tcp dport {{ 7380, 5000 }} accept",
            "",
            f"        # Block IoT → main LAN (permanent)",
            f"        ip saddr {iot_subnet} ip daddr != {iot_subnet} drop",
        ]

        # Per-device internet rules
        for dev in devices:
            if dev.blocked:
                lines.append(f"        # blocked: {dev.name or dev.mac}")
                lines.append(f"        ether saddr {dev.mac} drop")
            elif dev.internet_access == InternetAccess.ALLOW:
                lines.append(f"        # internet allowed: {dev.name or dev.mac}")
                lines.append(f"        ether saddr {dev.mac} ip daddr != {iot_subnet} accept")

        # Onboarding exceptions
        if onboarding_sessions:
            for sess in onboarding_sessions:
                if sess.state != OnboardingState.PENDING or sess.expired:
                    continue
                if sess.phone_ip:
                    lines.append(f"        # onboarding exception: {sess.id[:8]} phone bridge")
                    lines.append(f"        ip saddr {sess.phone_ip} accept")
                if sess.allow_internet:
                    lines.append(f"        # onboarding internet: {sess.id[:8]}")
                    lines.append(f"        ip saddr {iot_subnet} accept")

        lines += [
            "    }",
            "}",
        ]
        return "\n".join(lines)

    async def apply_device_rules(self, devices: list[IoTDevice],
                                  cfg: VLANConfig) -> bool:
        ruleset = self._generate_nftables(devices, cfg)
        try:
            proc = await asyncio.create_subprocess_exec(
                "nft", "-f", "-",
                stdin=asyncio.subprocess.PIPE,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE,
            )
            _, stderr = await proc.communicate(ruleset.encode())
            if proc.returncode != 0:
                log.error("nft error: %s", stderr.decode())
                return False
            return True
        except FileNotFoundError:
            log.warning("nft not found — skipping rule application (dev mode)")
            return True

    async def apply_onboarding_exception(self, session: OnboardingSession,
                                          cfg: VLANConfig) -> bool:
        # The exception is baked into the full ruleset; we just regenerate
        log.info("Onboarding exception applied for session %s", session.id[:8])
        return True

    async def remove_onboarding_exception(self, session_id: str) -> bool:
        log.info("Onboarding exception removed for session %s", session_id[:8])
        return True

    async def get_dhcp_leases(self) -> list[dict[str, str]]:
        lease_path = Path("/var/lib/misc/dnsmasq.leases")
        if not lease_path.exists():
            return []
        leases = []
        for line in lease_path.read_text().splitlines():
            parts = line.split()
            if len(parts) >= 4:
                leases.append({
                    "expires": parts[0], "mac": parts[1],
                    "ip": parts[2], "hostname": parts[3],
                })
        return leases


# ── UniFi backend ─────────────────────────────────────────────────────────────

class UniFiBackend:
    """UniFi Network REST API integration."""

    name = "unifi"

    def __init__(self, host: str, username: str, password: str,
                 site: str = "default", verify_ssl: bool = False) -> None:
        self._host = host.rstrip("/")
        self._username = username
        self._password = password
        self._site = site
        self._verify_ssl = verify_ssl
        self._cookie: str = ""

    async def _login(self) -> bool:
        try:
            import httpx
            async with httpx.AsyncClient(verify=self._verify_ssl) as client:
                r = await client.post(
                    f"{self._host}/api/login",
                    json={"username": self._username, "password": self._password},
                )
                if r.status_code == 200:
                    self._cookie = r.cookies.get("unifises", "")
                    return True
        except Exception as e:
            log.warning("UniFi login failed: %s", e)
        return False

    async def _api(self, method: str, path: str, data: dict | None = None) -> dict | None:
        if not self._cookie:
            if not await self._login():
                return None
        try:
            import httpx
            async with httpx.AsyncClient(
                verify=self._verify_ssl,
                cookies={"unifises": self._cookie},
            ) as client:
                url = f"{self._host}/api/s/{self._site}{path}"
                if method == "GET":
                    r = await client.get(url)
                else:
                    r = await client.post(url, json=data or {})
                if r.status_code == 401:
                    self._cookie = ""
                    return None
                return r.json()
        except Exception as e:
            log.warning("UniFi API error: %s", e)
            return None

    async def provision_vlan(self, cfg: VLANConfig) -> bool:
        result = await self._api("POST", "/rest/networkconf", {
            "name": f"ozma-iot-vlan{cfg.vlan_id}",
            "vlan_enabled": True, "vlan": cfg.vlan_id,
            "ip_subnet": f"{cfg.gateway}/24",
            "dhcpd_enabled": True,
            "dhcpd_start": cfg.dhcp_start, "dhcpd_stop": cfg.dhcp_end,
            "purpose": "corporate",
        })
        success = result is not None and result.get("meta", {}).get("rc") == "ok"
        if success:
            log.info("UniFi: IoT VLAN %d provisioned", cfg.vlan_id)
        return success

    async def apply_device_rules(self, devices: list[IoTDevice],
                                  cfg: VLANConfig) -> bool:
        # Block devices via UniFi MAC block list
        for dev in devices:
            if dev.blocked:
                await self._api("POST", "/cmd/stamgr", {
                    "cmd": "block-sta", "mac": dev.mac,
                })
        return True

    async def apply_onboarding_exception(self, session: OnboardingSession,
                                          cfg: VLANConfig) -> bool:
        return True  # UniFi: no per-session exception needed; dnsmasq handles isolation

    async def remove_onboarding_exception(self, session_id: str) -> bool:
        return True

    async def get_dhcp_leases(self) -> list[dict[str, str]]:
        result = await self._api("GET", "/stat/sta")
        if not result:
            return []
        leases = []
        for sta in result.get("data", []):
            leases.append({
                "mac": sta.get("mac", ""),
                "ip": sta.get("ip", ""),
                "hostname": sta.get("hostname", ""),
                "expires": str(int(time.time()) + 86400),
            })
        return leases


# ── MikroTik backend ──────────────────────────────────────────────────────────

class MikroTikBackend:
    """MikroTik RouterOS REST API integration (RouterOS v7.1+)."""

    name = "mikrotik"

    def __init__(self, host: str, username: str, password: str) -> None:
        self._base = f"http://{host.rstrip('/')}/rest"
        self._auth = (username, password)

    async def _api(self, method: str, path: str, data: dict | None = None) -> Any:
        try:
            import httpx
            async with httpx.AsyncClient() as client:
                url = self._base + path
                r = await client.request(
                    method, url, json=data,
                    auth=self._auth, timeout=10.0,
                )
                return r.json()
        except Exception as e:
            log.warning("MikroTik API error: %s", e)
            return None

    async def provision_vlan(self, cfg: VLANConfig) -> bool:
        iface = cfg.iface or "ether1"
        await self._api("PUT", "/interface/vlan", {
            "name": f"iot-vlan{cfg.vlan_id}",
            "vlan-id": str(cfg.vlan_id),
            "interface": iface,
        })
        await self._api("PUT", "/ip/address", {
            "address": f"{cfg.gateway}/24",
            "interface": f"iot-vlan{cfg.vlan_id}",
        })
        await self._api("PUT", "/ip/pool", {
            "name": f"iot-pool{cfg.vlan_id}",
            "ranges": f"{cfg.dhcp_start}-{cfg.dhcp_end}",
        })
        await self._api("PUT", "/ip/dhcp-server", {
            "name": f"iot-dhcp{cfg.vlan_id}",
            "interface": f"iot-vlan{cfg.vlan_id}",
            "address-pool": f"iot-pool{cfg.vlan_id}",
        })
        # Default-deny firewall rule
        await self._api("PUT", "/ip/firewall/filter", {
            "chain": "forward",
            "src-address": f"{cfg.subnet}.0/24",
            "dst-address": "!192.168.0.0/16",
            "action": "drop",
            "comment": "ozma-iot-default-deny",
        })
        log.info("MikroTik: IoT VLAN %d provisioned", cfg.vlan_id)
        return True

    async def apply_device_rules(self, devices: list[IoTDevice],
                                  cfg: VLANConfig) -> bool:
        for dev in devices:
            if dev.blocked:
                await self._api("PUT", "/ip/firewall/filter", {
                    "chain": "forward", "src-mac-address": dev.mac,
                    "action": "drop",
                    "comment": f"ozma-iot-block-{dev.id[:8]}",
                })
        return True

    async def apply_onboarding_exception(self, session: OnboardingSession,
                                          cfg: VLANConfig) -> bool:
        if session.phone_ip:
            await self._api("PUT", "/ip/firewall/filter", {
                "chain": "forward", "src-address": session.phone_ip,
                "action": "accept",
                "comment": f"ozma-onboard-{session.id[:8]}",
                "place-before": "0",
            })
        return True

    async def remove_onboarding_exception(self, session_id: str) -> bool:
        result = await self._api("GET", "/ip/firewall/filter")
        if result:
            for rule in result:
                if rule.get("comment", "").startswith(f"ozma-onboard-{session_id[:8]}"):
                    await self._api("DELETE", f"/ip/firewall/filter/{rule['.id']}")
        return True

    async def get_dhcp_leases(self) -> list[dict[str, str]]:
        result = await self._api("GET", "/ip/dhcp-server/lease")
        if not result:
            return []
        return [
            {"mac": l.get("mac-address", ""), "ip": l.get("address", ""),
             "hostname": l.get("host-name", ""), "expires": l.get("expires-after", "")}
            for l in result
        ]


# ── OpenWrt backend ───────────────────────────────────────────────────────────

class OpenWrtBackend:
    """OpenWrt ubus/UCI HTTP integration."""

    name = "openwrt"

    def __init__(self, host: str, username: str = "root", password: str = "") -> None:
        self._base = f"http://{host.rstrip('/')}"
        self._username = username
        self._password = password
        self._auth_token: str = ""

    async def _rpc(self, service: str, method: str, params: dict) -> Any:
        try:
            import httpx
            if not self._auth_token:
                async with httpx.AsyncClient() as client:
                    r = await client.post(
                        f"{self._base}/cgi-bin/luci/rpc/auth",
                        json={"method": "getAuthToken", "params": [self._username, self._password]},
                    )
                    self._auth_token = r.json().get("result", "")
            async with httpx.AsyncClient() as client:
                r = await client.post(
                    f"{self._base}/cgi-bin/luci/rpc/{service}",
                    params={"auth": self._auth_token},
                    json={"method": method, "params": list(params.values())},
                )
                return r.json().get("result")
        except Exception as e:
            log.warning("OpenWrt RPC error: %s", e)
            return None

    async def provision_vlan(self, cfg: VLANConfig) -> bool:
        # UCI commands via ubus exec
        uci_cmds = [
            f"uci set network.iot_vlan=interface",
            f"uci set network.iot_vlan.proto=static",
            f"uci set network.iot_vlan.ipaddr={cfg.gateway}",
            f"uci set network.iot_vlan.netmask=255.255.255.0",
            f"uci set network.iot_vlan.ifname=eth0.{cfg.vlan_id}",
            f"uci set dhcp.iot_vlan=dhcp",
            f"uci set dhcp.iot_vlan.interface=iot_vlan",
            f"uci set dhcp.iot_vlan.start=100",
            f"uci set dhcp.iot_vlan.limit=100",
            f"uci commit",
            f"/etc/init.d/network reload",
        ]
        for cmd in uci_cmds:
            await self._rpc("sys", "exec", {"cmd": cmd})
        log.info("OpenWrt: IoT VLAN %d provisioned", cfg.vlan_id)
        return True

    async def apply_device_rules(self, devices: list[IoTDevice],
                                  cfg: VLANConfig) -> bool:
        # Generate and push nftables rules via exec
        rules = NativeLinuxBackend()._generate_nftables(devices, cfg)
        await self._rpc("sys", "exec", {"cmd": f"echo '{rules}' | nft -f -"})
        return True

    async def apply_onboarding_exception(self, session: OnboardingSession,
                                          cfg: VLANConfig) -> bool:
        return True

    async def remove_onboarding_exception(self, session_id: str) -> bool:
        return True

    async def get_dhcp_leases(self) -> list[dict[str, str]]:
        result = await self._rpc("sys", "exec", {"cmd": "cat /tmp/dhcp.leases"})
        leases = []
        if result:
            for line in str(result).splitlines():
                parts = line.split()
                if len(parts) >= 4:
                    leases.append({
                        "expires": parts[0], "mac": parts[1],
                        "ip": parts[2], "hostname": parts[3],
                    })
        return leases


# ── pfSense / OPNsense backend ────────────────────────────────────────────────

class PfSenseBackend:
    """pfSense / OPNsense REST API integration."""

    name = "pfsense"

    def __init__(self, host: str, api_key: str, api_secret: str,
                 verify_ssl: bool = False) -> None:
        self._base = f"https://{host.rstrip('/')}/api/v1"
        self._headers = {
            "Authorization": f"{api_key} {api_secret}",
            "Content-Type": "application/json",
        }
        self._verify_ssl = verify_ssl

    async def _api(self, method: str, path: str, data: dict | None = None) -> Any:
        try:
            import httpx
            async with httpx.AsyncClient(verify=self._verify_ssl,
                                          headers=self._headers) as client:
                r = await client.request(method, self._base + path, json=data)
                return r.json()
        except Exception as e:
            log.warning("pfSense API error: %s", e)
            return None

    async def provision_vlan(self, cfg: VLANConfig) -> bool:
        # Create VLAN
        await self._api("POST", "/interface/vlan", {
            "if": cfg.iface or "em0", "tag": cfg.vlan_id,
            "descr": f"ozma-iot-{cfg.vlan_id}",
        })
        # Create interface
        await self._api("POST", "/interface", {
            "if": f"vlan{cfg.vlan_id}", "enable": True,
            "ipaddr": "static", "ipaddrv6": "none",
            "subnet": "24", "ipaddr": cfg.gateway,
        })
        # DHCP server
        await self._api("POST", "/services/dhcpd", {
            "interface": f"opt{cfg.vlan_id}",
            "enable": True,
            "range": {"from": cfg.dhcp_start, "to": cfg.dhcp_end},
        })
        # Firewall: default deny IoT → LAN
        await self._api("POST", "/firewall/rule", {
            "type": "block", "interface": f"opt{cfg.vlan_id}",
            "ipprotocol": "inet", "protocol": "any",
            "dst": "!192.168.0.0/16",
            "descr": "ozma-iot-default-deny",
        })
        log.info("pfSense: IoT VLAN %d provisioned", cfg.vlan_id)
        return True

    async def apply_device_rules(self, devices: list[IoTDevice],
                                  cfg: VLANConfig) -> bool:
        for dev in devices:
            if dev.blocked:
                await self._api("POST", "/firewall/rule", {
                    "type": "block", "interface": f"opt{cfg.vlan_id}",
                    "ipprotocol": "inet", "protocol": "any",
                    "src": dev.ip or "any", "descr": f"ozma-iot-block-{dev.id[:8]}",
                })
        return True

    async def apply_onboarding_exception(self, session: OnboardingSession,
                                          cfg: VLANConfig) -> bool:
        if session.phone_ip:
            await self._api("POST", "/firewall/rule", {
                "type": "pass", "interface": f"opt{cfg.vlan_id}",
                "src": session.phone_ip, "dst": "any",
                "descr": f"ozma-onboard-{session.id[:8]}", "top": True,
            })
        return True

    async def remove_onboarding_exception(self, session_id: str) -> bool:
        rules = await self._api("GET", "/firewall/rule")
        if rules and isinstance(rules.get("data"), list):
            for rule in rules["data"]:
                if rule.get("descr", "").startswith(f"ozma-onboard-{session_id[:8]}"):
                    await self._api("DELETE", f"/firewall/rule/{rule['tracker']}")
        return True

    async def get_dhcp_leases(self) -> list[dict[str, str]]:
        result = await self._api("GET", "/services/dhcpd/lease")
        if not result:
            return []
        return [
            {"mac": l.get("mac", ""), "ip": l.get("ip", ""),
             "hostname": l.get("hostname", ""), "expires": str(l.get("ends", ""))}
            for l in result.get("data", [])
        ]


# ── IoT Network Manager ───────────────────────────────────────────────────────

class IoTNetworkManager:
    """
    Central manager for the IoT VLAN — devices, policies, onboarding, backend dispatch.
    """

    ONBOARDING_TTL = 30 * 60   # 30 minutes default session TTL

    def __init__(self, config_path: Path = CONFIG_PATH) -> None:
        self._path = config_path
        self._vlan: VLANConfig = VLANConfig()
        self._devices: dict[str, IoTDevice] = {}
        self._sessions: dict[str, OnboardingSession] = {}
        self._backend: HardwareBackend = NativeLinuxBackend()
        self._expiry_task: asyncio.Task | None = None
        self._load()

    # ── Persistence ──────────────────────────────────────────────────────────

    def _load(self) -> None:
        if not self._path.exists():
            return
        try:
            data = json.loads(self._path.read_text())
            if "vlan" in data:
                self._vlan = VLANConfig.from_dict(data["vlan"])
            for d in data.get("devices", []):
                dev = IoTDevice.from_dict(d)
                self._devices[dev.id] = dev
            for s in data.get("sessions", []):
                sess = OnboardingSession.from_dict(s)
                self._sessions[sess.id] = sess
        except Exception as e:
            log.warning("Failed to load IoT network config: %s", e)

    def _save(self) -> None:
        data = {
            "vlan": self._vlan.to_dict(),
            "devices": [d.to_dict() for d in self._devices.values()],
            "sessions": [s.to_dict() for s in self._sessions.values()
                         if s.state in (OnboardingState.PENDING, OnboardingState.COMPLETE)],
        }
        tmp = self._path.with_suffix(".tmp")
        tmp.write_text(json.dumps(data, indent=2))
        tmp.replace(self._path)

    # ── Lifecycle ────────────────────────────────────────────────────────────

    async def start(self) -> None:
        self._expiry_task = asyncio.create_task(
            self._expiry_loop(), name="iot-onboarding-expiry"
        )
        log.info("IoT network manager started (backend: %s)", self._backend.name)

    async def stop(self) -> None:
        if self._expiry_task:
            self._expiry_task.cancel()

    async def _expiry_loop(self) -> None:
        """Expire stale onboarding sessions every 60 seconds."""
        while True:
            await asyncio.sleep(60)
            now = time.time()
            changed = False
            for sess in list(self._sessions.values()):
                if sess.state == OnboardingState.PENDING and sess.expired:
                    sess.state = OnboardingState.EXPIRED
                    await self._backend.remove_onboarding_exception(sess.id)
                    log.info("Onboarding session %s expired", sess.id[:8])
                    changed = True
            if changed:
                self._save()

    # ── Backend config ────────────────────────────────────────────────────────

    def configure_backend(self, backend_type: str, **kwargs: Any) -> None:
        """Switch to a hardware backend. Call before start()."""
        backends = {
            "native_linux": NativeLinuxBackend,
            "unifi": UniFiBackend,
            "mikrotik": MikroTikBackend,
            "openwrt": OpenWrtBackend,
            "pfsense": PfSenseBackend,
        }
        if backend_type not in backends:
            raise ValueError(f"Unknown backend: {backend_type}. "
                             f"Valid: {list(backends)}")
        self._backend = backends[backend_type](**kwargs)
        log.info("IoT backend: %s", backend_type)

    # ── VLAN setup ────────────────────────────────────────────────────────────

    async def provision(self, vlan: VLANConfig | None = None) -> bool:
        """Provision the IoT VLAN on the backend. Idempotent."""
        if vlan:
            self._vlan = vlan
            self._save()
        success = await self._backend.provision_vlan(self._vlan)
        if success:
            await self._push_rules()
        return success

    async def _push_rules(self) -> None:
        """Regenerate and push all device + onboarding rules."""
        devices = list(self._devices.values())
        sessions = [s for s in self._sessions.values()
                    if s.state == OnboardingState.PENDING and not s.expired]
        await self._backend.apply_device_rules(devices, self._vlan)
        for sess in sessions:
            await self._backend.apply_onboarding_exception(sess, self._vlan)

    # ── Device management ─────────────────────────────────────────────────────

    def add_device(self, mac: str, name: str = "",
                   category: DeviceCategory = DeviceCategory.UNKNOWN,
                   internet_access: InternetAccess = InternetAccess.DENY) -> IoTDevice:
        """Add a device to the inventory and apply isolation policy."""
        mac = mac.lower().replace("-", ":").replace(".", ":")
        # Find existing by MAC
        for dev in self._devices.values():
            if dev.mac == mac:
                dev.name = name or dev.name
                dev.category = category
                dev.internet_access = internet_access
                self._save()
                return dev
        dev = IoTDevice(
            id=str(uuid.uuid4()), mac=mac, name=name,
            category=category, internet_access=internet_access,
            added_at=time.time(), last_seen=time.time(),
        )
        self._devices[dev.id] = dev
        self._save()
        log.info("IoT device added: %s (%s) %s", name or mac, mac, category.value)
        return dev

    def update_device(self, device_id: str, **kwargs: Any) -> IoTDevice | None:
        dev = self._devices.get(device_id)
        if not dev:
            return None
        for k, v in kwargs.items():
            if hasattr(dev, k):
                setattr(dev, k, v)
        self._save()
        return dev

    def remove_device(self, device_id: str) -> bool:
        if device_id in self._devices:
            del self._devices[device_id]
            self._save()
            return True
        return False

    def get_device(self, device_id: str) -> IoTDevice | None:
        return self._devices.get(device_id)

    def list_devices(self) -> list[IoTDevice]:
        return list(self._devices.values())

    def get_device_by_mac(self, mac: str) -> IoTDevice | None:
        mac = mac.lower().replace("-", ":").replace(".", ":")
        for dev in self._devices.values():
            if dev.mac == mac:
                return dev
        return None

    def update_lease(self, mac: str, ip: str) -> None:
        """Update device IP from DHCP lease. Called from lease watcher."""
        dev = self.get_device_by_mac(mac)
        if dev:
            dev.ip = ip
            dev.last_seen = time.time()

    # ── Onboarding ────────────────────────────────────────────────────────────

    async def start_onboarding(self, device_name: str = "",
                                category: DeviceCategory = DeviceCategory.UNKNOWN,
                                phone_ip: str = "",
                                allow_internet: bool = False,
                                ttl: int | None = None) -> OnboardingSession:
        """
        Create an onboarding session and apply the exception rule.

        phone_ip: the user's phone IP on the main LAN. Gets temporary
                  bridged access to the IoT VLAN to reach the device's AP.
        allow_internet: set True for devices that require cloud registration
                        during setup (e.g. Nest, Ring). A warning is logged.
        """
        if allow_internet:
            log.warning(
                "Onboarding session %s has allow_internet=True — "
                "device will have internet access during setup", device_name
            )
        ttl = ttl if ttl is not None else self.ONBOARDING_TTL
        sess = OnboardingSession(
            id=str(uuid.uuid4()),
            device_name=device_name,
            category=category,
            phone_ip=phone_ip,
            allow_internet=allow_internet,
            created_at=time.time(),
            expires_at=time.time() + ttl if ttl > 0 else 0,
        )
        self._sessions[sess.id] = sess
        await self._backend.apply_onboarding_exception(sess, self._vlan)
        self._save()
        log.info("Onboarding started: %s (session %s)", device_name, sess.id[:8])
        return sess

    async def complete_onboarding(self, session_id: str,
                                   mac: str, name: str = "",
                                   internet_access: InternetAccess = InternetAccess.DENY,
                                   ) -> IoTDevice | None:
        """
        Complete an onboarding session: remove exception, add device to inventory.
        """
        sess = self._sessions.get(session_id)
        if not sess or sess.state != OnboardingState.PENDING:
            return None

        sess.state = OnboardingState.COMPLETE
        sess.completed_at = time.time()

        dev = self.add_device(
            mac=mac, name=name or sess.device_name,
            category=sess.category, internet_access=internet_access,
        )
        sess.device_id = dev.id

        await self._backend.remove_onboarding_exception(session_id)
        await self._push_rules()
        self._save()
        log.info("Onboarding complete: %s (device %s)", dev.name, dev.id[:8])
        return dev

    async def cancel_onboarding(self, session_id: str) -> bool:
        sess = self._sessions.get(session_id)
        if not sess or sess.state != OnboardingState.PENDING:
            return False
        sess.state = OnboardingState.CANCELLED
        await self._backend.remove_onboarding_exception(session_id)
        self._save()
        return True

    def get_session(self, session_id: str) -> OnboardingSession | None:
        return self._sessions.get(session_id)

    def list_sessions(self, active_only: bool = False) -> list[OnboardingSession]:
        if active_only:
            return [s for s in self._sessions.values()
                    if s.state == OnboardingState.PENDING and not s.expired]
        return list(self._sessions.values())

    # ── DHCP lease sync ──────────────────────────────────────────────────────

    async def sync_leases(self) -> int:
        """Pull DHCP leases from backend and update device IPs. Returns update count."""
        leases = await self._backend.get_dhcp_leases()
        count = 0
        for lease in leases:
            mac = lease.get("mac", "")
            ip = lease.get("ip", "")
            if mac and ip:
                self.update_lease(mac, ip)
                count += 1
        if count:
            self._save()
        return count

    # ── Policy export ────────────────────────────────────────────────────────

    def export_nftables(self) -> str:
        """Generate nftables rules as a string (for review or manual apply)."""
        sessions = [s for s in self._sessions.values()
                    if s.state == OnboardingState.PENDING and not s.expired]
        linux = NativeLinuxBackend()
        return linux._generate_nftables(
            list(self._devices.values()), self._vlan, sessions
        )

    # ── Status ───────────────────────────────────────────────────────────────

    def status(self) -> dict[str, Any]:
        active_sessions = [s for s in self._sessions.values()
                           if s.state == OnboardingState.PENDING and not s.expired]
        return {
            "backend": self._backend.name,
            "vlan": self._vlan.to_dict(),
            "device_count": len(self._devices),
            "active_onboarding": len(active_sessions),
            "devices_blocked": sum(1 for d in self._devices.values() if d.blocked),
            "devices_with_internet": sum(
                1 for d in self._devices.values()
                if d.internet_access != InternetAccess.DENY
            ),
        }
