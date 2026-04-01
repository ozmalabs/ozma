# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Node reconciliation — merge hardware and software nodes for the same machine.

When a hardware node (USB HID + audio gadget) and a software node (screen
capture + clipboard + display info) are both connected to the same target
machine, the controller merges them into a single logical node.

Auto-binding strategies (all checked, any match = bind):

  1. USB colocated: the soft node detects the ozma USB device on its host
     (VID/PID 1d6b:0104) and reports the hardware node's serial number.
     This works even when the nodes are on different networks — the USB
     cable is the proof of colocation.

  2. Same host IP: both nodes report the same target IP address.
     Works on flat networks. May false-match on NAT/VPN.

  3. Explicit config: user binds hw + sw node IDs in scenario config.
     Override for any edge case.

  4. Soft node self-report: the soft node announces which hardware node
     it's paired with (via mDNS TXT "hw_node=<id>" or registration field).

When nodes are on different networks, the reconciled node is flagged
with `split_network=True`. This is expected for servers (management
network vs production network) and remote nodes via Ozma Connect.
"""

from __future__ import annotations

import asyncio
import logging
import time
from dataclasses import dataclass, field
from typing import Any

from state import AppState, NodeInfo

log = logging.getLogger("ozma.reconciler")


@dataclass
class ReconciledNode:
    """A logical node combining hardware and software capabilities."""
    id: str                      # Canonical node ID (hardware node's)
    hardware_node_id: str = ""
    software_node_id: str = ""
    reconciled: bool = False
    split_network: bool = False  # True if hw and sw are on different networks
    bind_method: str = ""        # "usb", "ip", "explicit", "self_report", ""

    # Merged capabilities
    has_hid: bool = False
    has_audio: bool = False
    has_serial: bool = False
    has_power: bool = False
    has_screen_capture: bool = False
    has_clipboard: bool = False
    has_display_info: bool = False
    has_metrics: bool = False

    # Routing
    hid_target: str = ""         # Node ID to send HID packets to (hardware node)
    stream_source: str = ""      # Node ID to get screen stream from (software node)
    audio_source: str = ""       # Node ID for audio (hardware node preferred)

    def to_dict(self) -> dict:
        return {
            "id": self.id,
            "hardware_node": self.hardware_node_id,
            "software_node": self.software_node_id,
            "reconciled": self.reconciled,
            "split_network": self.split_network,
            "bind_method": self.bind_method,
            "capabilities": {
                "hid": self.has_hid,
                "audio": self.has_audio,
                "serial": self.has_serial,
                "power": self.has_power,
                "screen_capture": self.has_screen_capture,
                "clipboard": self.has_clipboard,
                "display_info": self.has_display_info,
                "metrics": self.has_metrics,
            },
            "routing": {
                "hid": self.hid_target,
                "stream": self.stream_source,
                "audio": self.audio_source,
            },
        }


class NodeReconciler:
    """
    Detects and merges hardware + software nodes for the same machine.

    Auto-binds using any available signal. Different networks are fine —
    this is expected for servers with management + production networks.
    """

    def __init__(self, state: AppState) -> None:
        self._state = state
        self._reconciled: dict[str, ReconciledNode] = {}
        self._explicit_bindings: dict[str, str] = {}  # hw_node_id → sw_node_id
        self._task: asyncio.Task | None = None

    async def start(self) -> None:
        self._task = asyncio.create_task(self._reconcile_loop(), name="node-reconciler")
        log.info("Node reconciler started")

    async def stop(self) -> None:
        if self._task:
            self._task.cancel()

    def bind(self, hardware_node_id: str, software_node_id: str) -> None:
        """Explicitly bind a hardware and software node."""
        self._explicit_bindings[hardware_node_id] = software_node_id
        log.info("Explicit binding: %s + %s", hardware_node_id, software_node_id)
        self._do_reconcile()

    def get_reconciled(self, node_id: str) -> ReconciledNode | None:
        if node_id in self._reconciled:
            return self._reconciled[node_id]
        for rn in self._reconciled.values():
            if rn.software_node_id == node_id or rn.hardware_node_id == node_id:
                return rn
        return None

    def list_reconciled(self) -> list[dict]:
        return [rn.to_dict() for rn in self._reconciled.values()]

    async def _reconcile_loop(self) -> None:
        while True:
            await asyncio.sleep(5)
            self._do_reconcile()

    def _do_reconcile(self) -> None:
        """Run reconciliation across all known nodes."""
        nodes = dict(self._state.nodes)
        hw_nodes: dict[str, NodeInfo] = {}
        sw_nodes: dict[str, NodeInfo] = {}

        for nid, node in nodes.items():
            caps = node.capabilities or []
            if "hid" in caps or "qmp" in caps:
                hw_nodes[nid] = node
            if "softnode" in caps:
                sw_nodes[nid] = node

        # Track which sw nodes are already bound
        bound_sw: set[str] = set()

        # Strategy 1: Explicit bindings (highest priority)
        for hw_id, sw_id in self._explicit_bindings.items():
            if hw_id in hw_nodes and sw_id in sw_nodes:
                self._merge(hw_id, sw_id, hw_nodes[hw_id], sw_nodes[sw_id], "explicit")
                bound_sw.add(sw_id)

        # Strategy 2: Soft node self-report (sw node announces its hw_node)
        for sw_id, sw_node in sw_nodes.items():
            if sw_id in bound_sw:
                continue
            hw_node_ref = getattr(sw_node, 'hw_node', None) or ""
            if hw_node_ref and hw_node_ref in hw_nodes and hw_node_ref not in self._reconciled:
                self._merge(hw_node_ref, sw_id, hw_nodes[hw_node_ref], sw_node, "self_report")
                bound_sw.add(sw_id)

        # Strategy 3: USB colocation (sw node reports ozma USB serial matching hw node)
        for sw_id, sw_node in sw_nodes.items():
            if sw_id in bound_sw:
                continue
            usb_serial = getattr(sw_node, 'colocated_usb_serial', None) or ""
            if usb_serial:
                for hw_id, hw_node in hw_nodes.items():
                    if hw_id in self._reconciled:
                        continue
                    # Match USB serial to hardware node's serial
                    hw_serial = getattr(hw_node, 'serial', '') or hw_id.split('.')[0]
                    if usb_serial == hw_serial:
                        self._merge(hw_id, sw_id, hw_node, sw_node, "usb")
                        bound_sw.add(sw_id)
                        break

        # Strategy 4: Same host IP (weakest signal but common case)
        for hw_id, hw_node in hw_nodes.items():
            if hw_id in self._reconciled:
                continue
            for sw_id, sw_node in sw_nodes.items():
                if sw_id in bound_sw:
                    continue
                if hw_node.host and sw_node.host and hw_node.host == sw_node.host:
                    self._merge(hw_id, sw_id, hw_node, sw_node, "ip")
                    bound_sw.add(sw_id)
                    break

        # Create standalone entries for unmatched nodes
        for hw_id, hw_node in hw_nodes.items():
            if hw_id not in self._reconciled:
                self._reconciled[hw_id] = self._from_hardware_only(hw_id, hw_node)

        for sw_id, sw_node in sw_nodes.items():
            if sw_id not in bound_sw and sw_id not in self._reconciled:
                already = any(rn.software_node_id == sw_id for rn in self._reconciled.values())
                if not already:
                    self._reconciled[sw_id] = self._from_software_only(sw_id, sw_node)

    def _merge(self, hw_id: str, sw_id: str, hw: NodeInfo, sw: NodeInfo,
               method: str) -> None:
        hw_caps = set(hw.capabilities or [])
        sw_caps = set(sw.capabilities or [])
        split = hw.host != sw.host

        rn = ReconciledNode(
            id=hw_id,
            hardware_node_id=hw_id,
            software_node_id=sw_id,
            reconciled=True,
            split_network=split,
            bind_method=method,
            has_hid="hid" in hw_caps or "qmp" in hw_caps,
            has_audio=bool(hw.audio_type),
            has_serial="serial" in hw_caps,
            has_power="power" in hw_caps,
            has_screen_capture="screen" in sw_caps or "capture" in sw_caps,
            has_clipboard="clipboard" in sw_caps,
            has_display_info="display" in sw_caps,
            has_metrics="metrics" in sw_caps,
            hid_target=hw_id,
            stream_source=sw_id,
            audio_source=hw_id if hw.audio_type else sw_id,
        )
        self._reconciled[hw_id] = rn

        network_note = " [SPLIT NETWORK]" if split else ""
        log.info("Reconciled (%s): %s (hw=%s @ %s + sw=%s @ %s)%s",
                 method, hw_id, hw_id, hw.host, sw_id, sw.host, network_note)

    def _from_hardware_only(self, hw_id: str, hw: NodeInfo) -> ReconciledNode:
        caps = set(hw.capabilities or [])
        return ReconciledNode(
            id=hw_id, hardware_node_id=hw_id,
            has_hid="hid" in caps or "qmp" in caps,
            has_audio=bool(hw.audio_type),
            has_serial="serial" in caps,
            has_power="power" in caps,
            has_screen_capture=bool(hw.vnc_host) or bool(getattr(hw, 'capture_device', None)),
            hid_target=hw_id, stream_source=hw_id, audio_source=hw_id,
        )

    def _from_software_only(self, sw_id: str, sw: NodeInfo) -> ReconciledNode:
        caps = set(sw.capabilities or [])
        return ReconciledNode(
            id=sw_id, software_node_id=sw_id,
            has_screen_capture="screen" in caps or "capture" in caps,
            has_clipboard="clipboard" in caps,
            has_display_info="display" in caps,
            has_metrics="metrics" in caps,
            has_hid="softnode" in caps,
            hid_target=sw_id, stream_source=sw_id,
            audio_source=sw_id if sw.audio_type else "",
        )
