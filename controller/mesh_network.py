# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Mesh networking — port forwarding, USB network card, distributed firewall.

Every node in the ozma mesh is a network endpoint. The mesh provides:

  1. Port forwarding: expose any port on any machine to any other machine
     in the mesh, or to the internet via Ozma Connect.

  2. USB Ethernet gadget: the hardware node presents as a USB network card
     to the target machine. The target gets an IP on the ozma mesh network.
     No other network connection required on the target.

  3. Distributed firewall: the controller enforces access rules across the
     mesh. Who can reach what, on which ports, from where. Centrally managed,
     per-node enforced.

Architecture:

  ┌──────────┐    USB Ethernet    ┌───────────┐    mesh    ┌──────────┐
  │ Target A │◄──(10.200.0.2)──►│  Node A    │◄─────────►│Controller│
  │ (no NIC) │   ECM/RNDIS      │(10.200.0.1)│           │(10.200.0.│
  └──────────┘                   └───────────┘            │  .254)   │
                                                          └────┬─────┘
  ┌──────────┐    USB Ethernet    ┌───────────┐    mesh        │
  │ Target B │◄──(10.200.1.2)──►│  Node B    │◄──────────────┘
  │ (server) │   ECM/RNDIS      │(10.200.1.1)│
  └──────────┘                   └───────────┘

  Target A can reach Target B at 10.200.1.2 — through the mesh.
  Target B can reach Target A at 10.200.0.2 — through the mesh.
  Neither target has any other network connection.

  Dual-stack: every node also gets a ULA IPv6 address (fdXX:XXXX:XXXX::/48).
  If the IPv4 mesh subnet collides with a local network (e.g. laptop moves
  to an office using 10.200.x.x), IPv6 ULA addresses still work.

  Via Ozma Connect:
  ┌──────────┐         ┌──────────┐         ┌──────────┐
  │ Internet │──HTTPS──│ Connect  │──relay──│Controller│──mesh──│Nodes│
  │ user     │         │ relay    │         │          │        │     │
  └──────────┘         └──────────┘         └──────────┘        └─────┘

  Port forward: expose Target B's port 3000 at
  https://myapp.connect.ozma.dev → routed through the mesh to Target B.

USB Ethernet gadget:
  Linux USB gadget ECM (Ethernet Control Model) or RNDIS (for Windows).
  The target machine sees a USB Ethernet adapter. DHCP server on the node
  assigns an IP from the mesh subnet. The node routes traffic between
  the USB network and the mesh.

  This means: plug an ozma node into ANY machine via USB. The machine
  gets network access. Through the mesh. With encryption. With firewall
  rules. Through one USB cable that also carries HID, audio, serial,
  and storage.

Port forwarding rules:
  {
    "id": "web-server",
    "source": "connect",              // or "mesh", or a specific node_id
    "target_node": "server-1",
    "target_port": 3000,
    "protocol": "tcp",
    "expose_as": "myapp.connect.ozma.dev",   // for Connect
    "expose_port": 443,                       // for mesh-internal
    "allowed_accounts": ["*"],                // or specific account IDs
    "enabled": true
  }

Firewall rules:
  {
    "id": "allow-ssh",
    "action": "allow",
    "source": {"node": "admin-desktop"},
    "target": {"node": "server-1", "port": 22},
    "protocol": "tcp"
  }
  Default: deny all inter-node traffic except HID/audio/control.
  Explicit allow rules for port forwarding and direct access.
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import secrets
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.mesh_network")

MESH_SUBNET = "10.200"  # 10.200.X.Y — X = node index, Y = device
CONFIG_PATH = Path(__file__).parent / "mesh_network.json"


# ── ULA IPv6 helpers (RFC 4193) ────────────────────────────────────────────

def generate_ula_prefix() -> str:
    """Generate a random /48 ULA prefix stem: fdXX:XXXX:XXXX."""
    global_id = secrets.token_bytes(5)  # 40 random bits
    b = b'\xfd' + global_id
    return f"{b[0]:02x}{b[1]:02x}:{b[2]:02x}{b[3]:02x}:{b[4]:02x}{b[5]:02x}"


def ula_node_ip(prefix: str, index: int, device: int = 1) -> str:
    """Derive a node/target IPv6 address from the ULA /48 prefix stem."""
    return f"{prefix}::{index:x}:{device}"


def ula_controller_ip(prefix: str) -> str:
    """Controller's mesh IPv6 address (::fe = 254)."""
    return f"{prefix}::fe"


@dataclass
class PortForward:
    """A port forwarding rule."""
    id: str
    source: str = "mesh"              # "mesh", "connect", or node_id
    target_node: str = ""             # node the traffic goes to
    target_host: str = ""             # IP on the target's USB network (or localhost)
    target_port: int = 0
    protocol: str = "tcp"             # tcp, udp
    expose_port: int = 0              # port exposed on the source side
    expose_domain: str = ""           # subdomain for Connect exposure
    allowed_accounts: list[str] = field(default_factory=lambda: ["*"])
    enabled: bool = True
    description: str = ""

    def to_dict(self) -> dict:
        return {
            "id": self.id, "source": self.source,
            "target_node": self.target_node, "target_port": self.target_port,
            "protocol": self.protocol, "expose_port": self.expose_port,
            "expose_domain": self.expose_domain, "enabled": self.enabled,
            "description": self.description,
        }


@dataclass
class FirewallRule:
    """A mesh firewall rule."""
    id: str
    action: str = "allow"             # allow, deny
    source_node: str = "*"            # node_id or "*" for any
    target_node: str = "*"
    target_port: int = 0              # 0 = any port
    protocol: str = "tcp"
    priority: int = 100               # lower = higher priority
    description: str = ""

    def to_dict(self) -> dict:
        return {
            "id": self.id, "action": self.action,
            "source": self.source_node, "target": self.target_node,
            "port": self.target_port, "protocol": self.protocol,
            "priority": self.priority, "description": self.description,
        }


@dataclass
class MeshNode:
    """A node's network identity in the mesh (dual-stack IPv4 + IPv6 ULA)."""
    node_id: str
    mesh_ip: str = ""                 # 10.200.X.1 (node's mesh IP)
    target_ip: str = ""               # 10.200.X.2 (target's USB network IP)
    subnet: str = ""                  # 10.200.X.0/24
    mesh_ip6: str = ""               # fdXX:XXXX:XXXX::X:1 (IPv6 ULA)
    target_ip6: str = ""             # fdXX:XXXX:XXXX::X:2
    subnet6: str = ""               # fdXX:XXXX:XXXX::/48
    usb_ethernet_active: bool = False
    index: int = 0

    def to_dict(self) -> dict:
        d = {
            "node_id": self.node_id, "mesh_ip": self.mesh_ip,
            "target_ip": self.target_ip, "subnet": self.subnet,
            "usb_ethernet_active": self.usb_ethernet_active,
            "index": self.index,
        }
        if self.mesh_ip6:
            d["mesh_ip6"] = self.mesh_ip6
            d["target_ip6"] = self.target_ip6
            d["subnet6"] = self.subnet6
        return d


class MeshNetworkManager:
    """
    Manages the ozma mesh network: IP allocation, port forwarding,
    firewall rules, and USB Ethernet gadget coordination.
    """

    def __init__(self) -> None:
        self._nodes: dict[str, MeshNode] = {}
        self._forwards: dict[str, PortForward] = {}
        self._firewall: list[FirewallRule] = []
        self._next_index = 1
        self._ula_prefix: str = ""  # e.g. "fd12:abcd:ef01" (/48 stem)
        self._load_config()

        # Default firewall: deny all, allow ozma control traffic
        if not self._firewall:
            self._firewall = [
                FirewallRule(id="default-deny", action="deny", priority=999,
                             description="Default deny all inter-node traffic"),
                FirewallRule(id="allow-ozma-control", action="allow",
                             target_port=7380, priority=10,
                             description="Allow ozma control traffic"),
                FirewallRule(id="allow-ozma-hid", action="allow",
                             target_port=7331, priority=10,
                             description="Allow ozma HID traffic"),
            ]

    def _load_config(self) -> None:
        if CONFIG_PATH.exists():
            try:
                data = json.loads(CONFIG_PATH.read_text())
                self._ula_prefix = data.get("ula_prefix", "")
                for n in data.get("nodes", []):
                    # Backward compat: old configs used "usb_ethernet" key
                    if "usb_ethernet" in n and "usb_ethernet_active" not in n:
                        n["usb_ethernet_active"] = n.pop("usb_ethernet")
                    mn = MeshNode(**{k: v for k, v in n.items()
                                    if k in MeshNode.__dataclass_fields__})
                    self._nodes[mn.node_id] = mn
                    self._next_index = max(self._next_index, mn.index + 1)
                for f in data.get("forwards", []):
                    pf = PortForward(**{k: v for k, v in f.items()
                                       if k in PortForward.__dataclass_fields__})
                    self._forwards[pf.id] = pf
                for r in data.get("firewall", []):
                    fr = FirewallRule(**{k: v for k, v in r.items()
                                        if k in FirewallRule.__dataclass_fields__})
                    self._firewall.append(fr)
            except Exception as e:
                log.error("Failed to load mesh config %s: %s", CONFIG_PATH, e)

        # Generate ULA prefix on first run or upgrade from pre-IPv6 config
        if not self._ula_prefix:
            self._ula_prefix = generate_ula_prefix()
            log.info("Generated ULA /48 prefix: %s::/48", self._ula_prefix)

        # Back-fill IPv6 addresses on nodes loaded from old configs
        backfilled = False
        for node in self._nodes.values():
            if not node.mesh_ip6 and node.index:
                node.mesh_ip6 = ula_node_ip(self._ula_prefix, node.index, 1)
                node.target_ip6 = ula_node_ip(self._ula_prefix, node.index, 2)
                node.subnet6 = f"{self._ula_prefix}::/48"
                backfilled = True
        if backfilled or not CONFIG_PATH.exists():
            self._save_config()

    def _save_config(self) -> None:
        data = {
            "ula_prefix": self._ula_prefix,
            "nodes": [n.to_dict() for n in self._nodes.values()],
            "forwards": [f.to_dict() for f in self._forwards.values()],
            "firewall": [r.to_dict() for r in self._firewall],
        }
        # Atomic write: write to temp file then rename (atomic on POSIX)
        tmp_path = CONFIG_PATH.with_suffix(CONFIG_PATH.suffix + ".tmp")
        tmp_path.write_text(json.dumps(data, indent=2))
        os.rename(tmp_path, CONFIG_PATH)

    # ── Node IP allocation ──────────────────────────────────────────────────

    def allocate_node(self, node_id: str) -> MeshNode:
        """Allocate mesh IPs for a node and its target machine."""
        if node_id in self._nodes:
            return self._nodes[node_id]

        idx = self._next_index
        if idx > 253:  # 10.200.X.Y — X must fit in one octet; .254 = controller
            raise ValueError(f"Mesh full: cannot allocate index {idx} (max 253 nodes)")
        self._next_index += 1

        node = MeshNode(
            node_id=node_id,
            mesh_ip=f"{MESH_SUBNET}.{idx}.1",
            target_ip=f"{MESH_SUBNET}.{idx}.2",
            subnet=f"{MESH_SUBNET}.{idx}.0/24",
            mesh_ip6=ula_node_ip(self._ula_prefix, idx, 1),
            target_ip6=ula_node_ip(self._ula_prefix, idx, 2),
            subnet6=f"{self._ula_prefix}::/48",
            index=idx,
        )
        self._nodes[node_id] = node
        self._save_config()
        log.info("Mesh IP allocated: %s → v4=%s v6=%s",
                 node_id, node.mesh_ip, node.mesh_ip6)
        return node

    def get_node_mesh(self, node_id: str) -> MeshNode | None:
        return self._nodes.get(node_id)

    # ── Port forwarding ─────────────────────────────────────────────────────

    def add_forward(self, forward: PortForward) -> None:
        self._forwards[forward.id] = forward
        self._save_config()
        log.info("Port forward added: %s → %s:%d (%s)",
                 forward.id, forward.target_node, forward.target_port, forward.source)

    def remove_forward(self, forward_id: str) -> bool:
        if forward_id in self._forwards:
            del self._forwards[forward_id]
            self._save_config()
            return True
        return False

    def list_forwards(self) -> list[dict]:
        return [f.to_dict() for f in self._forwards.values()]

    def get_forward(self, forward_id: str) -> PortForward | None:
        return self._forwards.get(forward_id)

    # ── Firewall ────────────────────────────────────────────────────────────

    def add_rule(self, rule: FirewallRule) -> None:
        self._firewall.append(rule)
        self._firewall.sort(key=lambda r: r.priority)
        self._save_config()

    def remove_rule(self, rule_id: str) -> bool:
        self._firewall = [r for r in self._firewall if r.id != rule_id]
        self._save_config()
        return True

    def list_rules(self) -> list[dict]:
        return [r.to_dict() for r in self._firewall]

    def check_access(self, source_node: str, target_node: str,
                     target_port: int, protocol: str = "tcp") -> bool:
        """Check if a connection is allowed by the firewall rules."""
        for rule in self._firewall:
            # Check if rule matches
            src_match = rule.source_node == "*" or rule.source_node == source_node
            tgt_match = rule.target_node == "*" or rule.target_node == target_node
            port_match = rule.target_port == 0 or rule.target_port == target_port
            proto_match = rule.protocol == protocol or rule.protocol == "*"

            if src_match and tgt_match and port_match and proto_match:
                return rule.action == "allow"

        return False  # default deny

    # ── USB Ethernet gadget config generation ───────────────────────────────

    def generate_usb_ethernet_config(self, node_id: str) -> dict | None:
        """
        Generate the USB Ethernet gadget configuration for a node.

        Returns the config needed to set up ECM/RNDIS on the node,
        including the IP addresses and DHCP range for the target.
        """
        node = self._nodes.get(node_id)
        if not node:
            return None

        cfg = {
            "gadget_type": "ecm",  # or "rndis" for Windows targets
            "node_ip": node.mesh_ip,
            "target_ip": node.target_ip,
            "subnet_mask": "255.255.255.0",
            "dhcp_range_start": node.target_ip,
            "dhcp_range_end": node.target_ip,  # single IP — only the target
            "dns": node.mesh_ip,  # node acts as DNS proxy
            "gateway": node.mesh_ip,  # node routes to the mesh
            "mtu": 1400,  # slightly under 1500 for encapsulation overhead
        }
        if node.mesh_ip6:
            cfg["node_ip6"] = node.mesh_ip6
            cfg["target_ip6"] = node.target_ip6
            cfg["prefix_len6"] = 64
        return cfg

    # ── Properties ──────────────────────────────────────────────────────────

    @property
    def ula_prefix(self) -> str:
        """The mesh ULA /48 prefix stem (e.g. 'fd12:abcd:ef01')."""
        return self._ula_prefix

    def bypass_subnets(self) -> list[str]:
        """Subnets that should bypass auth (mesh traffic). Dual-stack."""
        subnets = [f"{MESH_SUBNET}.0.0/16"]
        if self._ula_prefix:
            subnets.append(f"{self._ula_prefix}::/48")
        return subnets

    # ── Status ──────────────────────────────────────────────────────────────

    def status(self) -> dict:
        return {
            "nodes": [n.to_dict() for n in self._nodes.values()],
            "forwards": self.list_forwards(),
            "firewall_rules": len(self._firewall),
            "mesh_subnet": f"{MESH_SUBNET}.0.0/16",
            "mesh_subnet6": f"{self._ula_prefix}::/48",
            "ula_prefix": self._ula_prefix,
        }
