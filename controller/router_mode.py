# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Router mode — NAT, DHCP, DNS, nftables gateway operation.

When enabled, the controller acts as a full IP gateway for the IoT VLAN
and optionally for the main LAN.  This is the "complete appliance" path:
one N150 box does routing + KVM + NVR + media + HA.

Architecture
────────────
  WAN interface (eth0)       — upstream, DHCP client or static
  LAN interface (eth1)       — downstream, controller is the gateway
  IoT interface (eth1.20)    — VLAN 20 sub-interface, IoT devices

  NAT: nftables masquerade on WAN
  DHCP: dnsmasq (main LAN + IoT VLAN, separate scopes)
  DNS: dnsmasq (upstream: system resolvers or custom)
  Forwarding: sysctl net.ipv4.ip_forward=1

IoT isolation via nftables
──────────────────────────
  IoT → WAN: deny (default)
  IoT → LAN: deny
  IoT → Controller 7380: allow (API)
  IoT → Frigate 5000: allow (camera feeds)
  Per-device allow rules: added by IoTNetworkManager when internet access is granted

Camera VLAN exemption
─────────────────────
  Camera nodes running Ozma firmware use the mesh CA and encrypted transport.
  They do not need the IoT VLAN — they can run on the main LAN or on any VLAN
  since their channel is already authenticated and encrypted.  When a node
  registers with machine_class="camera", this module adds a rule allowing its
  IP full controller access without VLAN restriction.

Usage
─────
    router = RouterModeManager()
    await router.start()       # applies nftables + dnsmasq if enabled
    await router.stop()        # flushes rules, stops dnsmasq
"""

from __future__ import annotations

import asyncio
import logging
import os
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.router_mode")

DNSMASQ_CONF_PATH = Path("/tmp/ozma-dnsmasq.conf")
DNSMASQ_PID_PATH  = Path("/tmp/ozma-dnsmasq.pid")
NFT_TABLE_NAME    = "ozma_router"
ROUTER_STATE_PATH = Path(__file__).parent / "router_mode_state.json"


# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

@dataclass
class RouterConfig:
    enabled: bool = False

    # Network interfaces
    wan_interface: str = "eth0"
    lan_interface: str = "eth1"
    iot_vlan_id: int = 20

    # LAN addressing
    lan_subnet: str = "192.168.1.0/24"
    lan_gateway: str = "192.168.1.1"      # controller IP on LAN
    lan_dhcp_start: str = "192.168.1.100"
    lan_dhcp_end: str = "192.168.1.200"
    lan_dhcp_lease: str = "24h"

    # IoT VLAN addressing
    iot_subnet: str = "192.168.20.0/24"
    iot_gateway: str = "192.168.20.1"
    iot_dhcp_start: str = "192.168.20.100"
    iot_dhcp_end: str = "192.168.20.200"
    iot_dhcp_lease: str = "12h"

    # DNS
    upstream_dns: list[str] = field(default_factory=lambda: ["1.1.1.1", "1.0.0.1"])

    # Camera VLAN exemption: list of trusted camera node IPs (full access)
    trusted_camera_ips: list[str] = field(default_factory=list)

    # Per-device outbound cloud allow rules: {"ip": "...", "destination": "...", "comment": "..."}
    cloud_allow_rules: list[dict] = field(default_factory=list)

    def to_dict(self) -> dict:
        return {
            "enabled": self.enabled,
            "wan_interface": self.wan_interface,
            "lan_interface": self.lan_interface,
            "iot_vlan_id": self.iot_vlan_id,
            "lan_subnet": self.lan_subnet,
            "lan_gateway": self.lan_gateway,
            "lan_dhcp_start": self.lan_dhcp_start,
            "lan_dhcp_end": self.lan_dhcp_end,
            "lan_dhcp_lease": self.lan_dhcp_lease,
            "iot_subnet": self.iot_subnet,
            "iot_gateway": self.iot_gateway,
            "iot_dhcp_start": self.iot_dhcp_start,
            "iot_dhcp_end": self.iot_dhcp_end,
            "iot_dhcp_lease": self.iot_dhcp_lease,
            "upstream_dns": self.upstream_dns,
            "trusted_camera_ips": self.trusted_camera_ips,
            "cloud_allow_rules": self.cloud_allow_rules,
        }

    @classmethod
    def from_dict(cls, d: dict) -> "RouterConfig":
        return cls(
            enabled=d.get("enabled", False),
            wan_interface=d.get("wan_interface", "eth0"),
            lan_interface=d.get("lan_interface", "eth1"),
            iot_vlan_id=int(d.get("iot_vlan_id", 20)),
            lan_subnet=d.get("lan_subnet", "192.168.1.0/24"),
            lan_gateway=d.get("lan_gateway", "192.168.1.1"),
            lan_dhcp_start=d.get("lan_dhcp_start", "192.168.1.100"),
            lan_dhcp_end=d.get("lan_dhcp_end", "192.168.1.200"),
            lan_dhcp_lease=d.get("lan_dhcp_lease", "24h"),
            iot_subnet=d.get("iot_subnet", "192.168.20.0/24"),
            iot_gateway=d.get("iot_gateway", "192.168.20.1"),
            iot_dhcp_start=d.get("iot_dhcp_start", "192.168.20.100"),
            iot_dhcp_end=d.get("iot_dhcp_end", "192.168.20.200"),
            iot_dhcp_lease=d.get("iot_dhcp_lease", "12h"),
            upstream_dns=d.get("upstream_dns", ["1.1.1.1", "1.0.0.1"]),
            trusted_camera_ips=d.get("trusted_camera_ips", []),
            cloud_allow_rules=d.get("cloud_allow_rules", []),
        )


# ---------------------------------------------------------------------------
# nftables config generation
# ---------------------------------------------------------------------------

def _build_nft_ruleset(cfg: RouterConfig) -> str:
    """
    Generate the nftables ruleset for router + IoT isolation.

    The ruleset is idempotent: it flushes and recreates the ozma_router table.
    """
    iot_iface = f"{cfg.lan_interface}.{cfg.iot_vlan_id}"
    wan = cfg.wan_interface
    lan = cfg.lan_interface
    controller_ip = cfg.lan_gateway  # controller is the gateway

    # Cloud allow rules
    cloud_allow_lines = ""
    for rule in cfg.cloud_allow_rules:
        ip = rule.get("ip", "")
        dest = rule.get("destination", "")
        comment = rule.get("comment", "")
        if ip and dest:
            cloud_allow_lines += (
                f'        # {comment}\n'
                f'        ip saddr {ip} ip daddr {dest} accept\n'
            )

    # Trusted camera IPs: grant full access (they use authenticated Ozma transport)
    camera_trust_lines = ""
    for cam_ip in cfg.trusted_camera_ips:
        camera_trust_lines += f"        ip saddr {cam_ip} accept\n"

    return f"""#!/usr/sbin/nft -f

table inet {NFT_TABLE_NAME} {{

    # NAT: masquerade all LAN/IoT traffic going out through WAN
    chain POSTROUTING {{
        type nat hook postrouting priority 100; policy accept;
        oifname "{wan}" masquerade
    }}

    # Forward chain: LAN ↔ WAN allowed; IoT strictly controlled
    chain FORWARD {{
        type filter hook forward priority 0; policy drop;

        # Established/related connections always pass
        ct state established,related accept

        # LAN → WAN: allow full access
        iifname "{lan}" oifname "{wan}" accept
        iifname "{wan}" oifname "{lan}" ct state established,related accept

        # Trusted camera nodes: bypass IoT restrictions (mesh CA authenticated)
{camera_trust_lines}
        # Per-device cloud allow rules (explicit outbound, audit-logged)
{cloud_allow_lines}
        # IoT → controller API (7380) and Frigate (5000): always allowed
        iifname "{iot_iface}" ip daddr {controller_ip} tcp dport {{ 5000, 7380 }} accept

        # IoT → WAN: DENY by default (per-device rules above override)
        iifname "{iot_iface}" oifname "{wan}" drop

        # IoT → LAN: DENY (prevent lateral movement)
        iifname "{iot_iface}" oifname "{lan}" drop
    }}

    # Input chain: protect the controller itself
    chain INPUT {{
        type filter hook input priority 0; policy accept;
        ct state established,related accept
    }}
}}
"""


# ---------------------------------------------------------------------------
# dnsmasq config generation
# ---------------------------------------------------------------------------

def _build_dnsmasq_conf(cfg: RouterConfig, dns_filter_conf_dir: str | None = None) -> str:
    iot_iface = f"{cfg.lan_interface}.{cfg.iot_vlan_id}"
    conf_dir_line = ""
    if dns_filter_conf_dir:
        conf_dir_line = f"\n# DNS filter blocklist (written by DNSFilterManager)\nconf-dir={dns_filter_conf_dir},*.conf\n"
    return f"""# Ozma router mode — dnsmasq configuration
pid-file={DNSMASQ_PID_PATH}
no-resolv
server={",".join(cfg.upstream_dns)}
interface={cfg.lan_interface}
interface={iot_iface}
{conf_dir_line}
# LAN DHCP
dhcp-range={cfg.lan_interface},{cfg.lan_dhcp_start},{cfg.lan_dhcp_end},{cfg.lan_dhcp_lease}

# IoT VLAN DHCP
dhcp-range={iot_iface},{cfg.iot_dhcp_start},{cfg.iot_dhcp_end},{cfg.iot_dhcp_lease}
"""


# ---------------------------------------------------------------------------
# Manager
# ---------------------------------------------------------------------------

class RouterModeManager:
    """Manages router mode: NAT + DHCP + DNS + IoT firewall."""

    def __init__(self, state_path: Path = ROUTER_STATE_PATH,
                 dns_filter_conf_dir: str | None = None) -> None:
        self._state_path = state_path
        self._config = RouterConfig()
        self._dnsmasq_proc: asyncio.subprocess.Process | None = None
        self._active = False
        self._dns_filter_conf_dir = dns_filter_conf_dir  # injected by main.py
        self._load()

    # ------------------------------------------------------------------
    # Lifecycle
    # ------------------------------------------------------------------

    async def start(self) -> None:
        self._load()
        if self._config.enabled:
            await self._apply()
        log.info("RouterModeManager started (enabled=%s)", self._config.enabled)

    async def stop(self) -> None:
        await self._teardown()
        log.info("RouterModeManager stopped")

    async def _apply(self) -> None:
        """Enable IP forwarding, apply nftables, start dnsmasq."""
        await self._enable_ip_forwarding()
        await self._apply_vlan_interface()
        await self._apply_nftables()
        await self._start_dnsmasq()
        self._active = True
        log.info("Router mode active: %s → %s (IoT VLAN %d)",
                 self._config.wan_interface, self._config.lan_interface,
                 self._config.iot_vlan_id)

    async def _teardown(self) -> None:
        """Stop dnsmasq and flush nftables rules."""
        await self._stop_dnsmasq()
        await self._flush_nftables()
        self._active = False

    # ------------------------------------------------------------------
    # Sub-operations
    # ------------------------------------------------------------------

    async def _enable_ip_forwarding(self) -> None:
        try:
            await self._run("sysctl", "-w", "net.ipv4.ip_forward=1")
        except Exception as exc:
            log.error("Failed to enable IP forwarding: %s", exc)

    async def _apply_vlan_interface(self) -> None:
        """Create the IoT VLAN sub-interface if it doesn't exist."""
        iot_iface = f"{self._config.lan_interface}.{self._config.iot_vlan_id}"
        try:
            # Check if already exists
            proc = await asyncio.create_subprocess_exec(
                "ip", "link", "show", iot_iface,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.DEVNULL,
            )
            if await proc.wait() == 0:
                return  # already exists
            await self._run(
                "ip", "link", "add", "link", self._config.lan_interface,
                "name", iot_iface, "type", "vlan", "id", str(self._config.iot_vlan_id),
            )
            await self._run("ip", "addr", "add", f"{self._config.iot_gateway}/24", "dev", iot_iface)
            await self._run("ip", "link", "set", iot_iface, "up")
        except Exception as exc:
            log.warning("Could not create VLAN interface %s: %s", iot_iface, exc)

    async def _apply_nftables(self) -> None:
        ruleset = _build_nft_ruleset(self._config)
        nft_file = Path("/tmp/ozma-router.nft")
        nft_file.write_text(ruleset)
        nft_file.chmod(0o600)
        try:
            await self._run("nft", "-f", str(nft_file))
        except Exception as exc:
            log.error("nftables apply failed: %s", exc)
        finally:
            nft_file.unlink(missing_ok=True)

    async def _flush_nftables(self) -> None:
        try:
            await self._run("nft", "delete", "table", "inet", NFT_TABLE_NAME)
        except Exception:
            pass  # table may not exist

    async def _start_dnsmasq(self) -> None:
        conf = _build_dnsmasq_conf(self._config, self._dns_filter_conf_dir)
        DNSMASQ_CONF_PATH.write_text(conf)
        DNSMASQ_CONF_PATH.chmod(0o600)
        try:
            self._dnsmasq_proc = await asyncio.create_subprocess_exec(
                "dnsmasq", f"--conf-file={DNSMASQ_CONF_PATH}", "--no-daemon",
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE,
            )
            log.info("dnsmasq started (pid=%d)", self._dnsmasq_proc.pid)
        except FileNotFoundError:
            log.warning("dnsmasq not installed — DHCP/DNS not available (install dnsmasq)")
        except Exception as exc:
            log.error("dnsmasq failed to start: %s", exc)

    async def _stop_dnsmasq(self) -> None:
        if self._dnsmasq_proc and self._dnsmasq_proc.returncode is None:
            self._dnsmasq_proc.terminate()
            try:
                await asyncio.wait_for(self._dnsmasq_proc.wait(), timeout=5.0)
            except asyncio.TimeoutError:
                self._dnsmasq_proc.kill()
        self._dnsmasq_proc = None
        DNSMASQ_CONF_PATH.unlink(missing_ok=True)

    @staticmethod
    async def _run(*args: str) -> None:
        proc = await asyncio.create_subprocess_exec(
            *args,
            stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.PIPE,
        )
        _, err = await proc.communicate()
        if proc.returncode != 0:
            raise RuntimeError(f"{args[0]} failed: {err.decode(errors='replace').strip()}")

    # ------------------------------------------------------------------
    # Camera VLAN exemption
    # ------------------------------------------------------------------

    async def add_trusted_camera(self, ip: str) -> None:
        """
        Grant a camera node full access, bypassing IoT VLAN restrictions.

        Called by IoTNetworkManager / main.py when a camera node with
        machine_class="camera" registers — Ozma camera nodes are trusted
        via mesh CA + encrypted transport and do not need VLAN isolation.
        """
        if ip in self._config.trusted_camera_ips:
            return
        self._config.trusted_camera_ips.append(ip)
        self._save()
        if self._active:
            await self._apply_nftables()
        log.info("Camera node %s added to trusted (VLAN exempt)", ip)

    async def remove_trusted_camera(self, ip: str) -> None:
        if ip not in self._config.trusted_camera_ips:
            return
        self._config.trusted_camera_ips.remove(ip)
        self._save()
        if self._active:
            await self._apply_nftables()

    # ------------------------------------------------------------------
    # Per-device cloud allow rules
    # ------------------------------------------------------------------

    async def add_cloud_allow_rule(
        self,
        device_ip: str,
        destination: str,
        comment: str = "",
    ) -> dict:
        """
        Allow a specific IoT device to reach its vendor cloud endpoint.

        Each rule is audited: added to cloud_allow_rules with a timestamp.
        The firewall is reloaded immediately.
        """
        import time
        rule = {
            "ip": device_ip,
            "destination": destination,
            "comment": comment,
            "added_at": time.time(),
        }
        self._config.cloud_allow_rules.append(rule)
        self._save()
        if self._active:
            await self._apply_nftables()
        log.info("Cloud allow rule added: %s → %s (%s)", device_ip, destination, comment)
        return rule

    async def remove_cloud_allow_rule(self, device_ip: str, destination: str) -> bool:
        before = len(self._config.cloud_allow_rules)
        self._config.cloud_allow_rules = [
            r for r in self._config.cloud_allow_rules
            if not (r["ip"] == device_ip and r["destination"] == destination)
        ]
        if len(self._config.cloud_allow_rules) == before:
            return False
        self._save()
        if self._active:
            await self._apply_nftables()
        return True

    def list_cloud_allow_rules(self) -> list[dict]:
        return list(self._config.cloud_allow_rules)

    # ------------------------------------------------------------------
    # Configuration
    # ------------------------------------------------------------------

    def get_config(self) -> RouterConfig:
        return self._config

    async def set_config(self, **updates) -> RouterConfig:
        was_enabled = self._config.enabled
        for key, value in updates.items():
            if hasattr(self._config, key):
                setattr(self._config, key, value)
        self._save()
        if self._config.enabled and not was_enabled:
            await self._apply()
        elif was_enabled and not self._config.enabled:
            await self._teardown()
        elif self._config.enabled and self._active:
            # Re-apply with updated config
            await self._teardown()
            await self._apply()
        return self._config

    def get_status(self) -> dict:
        return {
            "enabled": self._config.enabled,
            "active": self._active,
            "wan_interface": self._config.wan_interface,
            "lan_interface": self._config.lan_interface,
            "iot_vlan_id": self._config.iot_vlan_id,
            "lan_gateway": self._config.lan_gateway,
            "iot_gateway": self._config.iot_gateway,
            "trusted_camera_count": len(self._config.trusted_camera_ips),
            "cloud_allow_rules": len(self._config.cloud_allow_rules),
        }

    # ------------------------------------------------------------------
    # Frigate auto-start on capable hardware
    # ------------------------------------------------------------------

    async def start_frigate_if_capable(self) -> bool:
        """
        Detect if this host can run Frigate (GPU/NPU + enough RAM) and start it.

        Returns True if Frigate was started or was already running.

        Detection criteria:
          - Docker available
          - ≥ 4 GB RAM
          - GPU/NPU detected (NVIDIA, AMD, Intel QSV, rknpu2, hailo)
        """
        if not await self._docker_available():
            log.debug("Docker not available — Frigate auto-start skipped")
            return False

        ram_gb = self._get_ram_gb()
        if ram_gb < 4:
            log.debug("Insufficient RAM (%.1f GB < 4 GB) — Frigate auto-start skipped", ram_gb)
            return False

        accel = await self._detect_accelerator()
        if not accel:
            log.debug("No GPU/NPU detected — Frigate auto-start skipped")
            return False

        # Check if already running
        proc = await asyncio.create_subprocess_exec(
            "docker", "ps", "--filter", "name=frigate", "--format", "{{.Names}}",
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.DEVNULL,
        )
        out, _ = await proc.communicate()
        if b"frigate" in out:
            log.info("Frigate already running")
            return True

        # Start Frigate with detected accelerator
        log.info("Auto-starting Frigate (accelerator: %s, RAM: %.1f GB)", accel, ram_gb)
        detector_flag = self._detector_flag(accel)
        try:
            await asyncio.create_subprocess_exec(
                "docker", "run", "-d",
                "--name", "frigate",
                "--restart", "unless-stopped",
                "--privileged",
                "-p", "5000:5000",
                "-p", "8554:8554",
                "-e", f"FRIGATE_DEFAULT_DETECTOR={detector_flag}",
                "ghcr.io/blakeblackshear/frigate:stable",
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.DEVNULL,
            )
            log.info("Frigate started via Docker")
            return True
        except Exception as exc:
            log.error("Failed to start Frigate: %s", exc)
            return False

    @staticmethod
    async def _docker_available() -> bool:
        try:
            proc = await asyncio.create_subprocess_exec(
                "docker", "info",
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.DEVNULL,
            )
            return await proc.wait() == 0
        except FileNotFoundError:
            return False

    @staticmethod
    def _get_ram_gb() -> float:
        try:
            meminfo = Path("/proc/meminfo").read_text()
            for line in meminfo.splitlines():
                if line.startswith("MemTotal:"):
                    kb = int(line.split()[1])
                    return kb / (1024 * 1024)
        except Exception:
            pass
        return 0.0

    @staticmethod
    async def _detect_accelerator() -> str | None:
        """Return the best available accelerator type or None."""
        # Check NVIDIA
        try:
            proc = await asyncio.create_subprocess_exec(
                "nvidia-smi", "--query-gpu=name", "--format=csv,noheader",
                stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL,
            )
            out, _ = await proc.communicate()
            if proc.returncode == 0 and out.strip():
                return "nvidia"
        except FileNotFoundError:
            pass

        # Check Intel QSV
        if Path("/dev/dri/render128").exists() or Path("/dev/dri/renderD128").exists():
            return "intel"

        # Check Rockchip NPU (rknpu2)
        if Path("/dev/dri").exists():
            for p in Path("/dev/dri").glob("*"):
                if "rk" in p.name.lower():
                    return "rknpu2"

        # Check Hailo
        if Path("/dev/hailo0").exists():
            return "hailo"

        # AMD ROCm
        if Path("/dev/kfd").exists():
            return "amd"

        return None

    @staticmethod
    def _detector_flag(accel: str) -> str:
        return {
            "nvidia": "cuda",
            "intel": "openvino",
            "rknpu2": "rknpu2",
            "hailo": "hailo8l",
            "amd": "rocm",
        }.get(accel, "cpu")

    # ------------------------------------------------------------------
    # Persistence
    # ------------------------------------------------------------------

    def _save(self) -> None:
        import json
        tmp = self._state_path.with_suffix(".tmp")
        self._state_path.parent.mkdir(parents=True, exist_ok=True)
        tmp.write_text(json.dumps(self._config.to_dict(), indent=2))
        tmp.chmod(0o600)
        tmp.rename(self._state_path)

    def _load(self) -> None:
        import json
        if not self._state_path.exists():
            return
        try:
            data = json.loads(self._state_path.read_text())
            self._config = RouterConfig.from_dict(data)
        except Exception:
            log.exception("Failed to load router mode state")
