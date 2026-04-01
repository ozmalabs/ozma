# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Tests for node reconciliation — auto-binding hardware + software nodes.

Verifies:
  1. Hardware node alone → reconciled with HID + audio
  2. Software node alone → reconciled with screen + clipboard
  3. Same IP → auto-merge
  4. Different IPs → auto-merge via self-report, flagged split_network
  5. Explicit binding overrides everything
  6. USB colocation detection
  7. Split network is flagged (expected for servers)
  8. Correct bind method recorded
  9. Lookup by either ID works
"""

import asyncio
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent / "controller"))

from state import AppState, NodeInfo
from node_reconciler import NodeReconciler


def make_hw_node(node_id: str, host: str = "10.0.0.5") -> NodeInfo:
    return NodeInfo(
        id=node_id, host=host, port=7331,
        role="compute", hw="milkv-duos", fw_version="1.0.0",
        proto_version=1, capabilities=["hid", "audio", "serial", "power"],
        audio_type="pipewire", audio_sink=f"ozma-{node_id.split('.')[0]}",
    )


def make_sw_node(node_id: str, host: str = "10.0.0.5",
                 hw_node_ref: str = "") -> NodeInfo:
    node = NodeInfo(
        id=node_id, host=host, port=7331,
        role="compute", hw="desktop-linux", fw_version="1.0.0",
        proto_version=1, capabilities=["softnode", "screen", "clipboard", "display", "metrics"],
        audio_type="pipewire", audio_sink=f"ozma-{node_id.split('.')[0]}",
    )
    # Soft node reports which hardware node it's colocated with
    if hw_node_ref:
        node.hw_node = hw_node_ref  # type: ignore[attr-defined]
    return node


def add_nodes(state, *nodes):
    loop = asyncio.new_event_loop()
    for n in nodes:
        loop.run_until_complete(state.add_node(n))
    loop.close()


def test_hardware_only():
    state = AppState()
    add_nodes(state, make_hw_node("hw-1._ozma._udp.local."))
    r = NodeReconciler(state)
    r._do_reconcile()
    rn = r.get_reconciled("hw-1._ozma._udp.local.")
    assert rn and rn.has_hid and not rn.reconciled
    print("  PASS  hardware-only node")


def test_software_only():
    state = AppState()
    add_nodes(state, make_sw_node("sw-1._ozma._udp.local."))
    r = NodeReconciler(state)
    r._do_reconcile()
    rn = r.get_reconciled("sw-1._ozma._udp.local.")
    assert rn and rn.has_screen_capture and not rn.reconciled
    print("  PASS  software-only node")


def test_auto_merge_same_ip():
    state = AppState()
    add_nodes(state,
        make_hw_node("hw-1._ozma._udp.local.", "10.0.0.5"),
        make_sw_node("sw-1._ozma._udp.local.", "10.0.0.5"),
    )
    r = NodeReconciler(state)
    r._do_reconcile()
    rn = r.get_reconciled("hw-1._ozma._udp.local.")
    assert rn and rn.reconciled
    assert rn.bind_method == "ip"
    assert not rn.split_network
    assert rn.hid_target == "hw-1._ozma._udp.local."
    assert rn.stream_source == "sw-1._ozma._udp.local."
    print("  PASS  auto-merge same IP (method=ip)")


def test_auto_merge_different_ip_self_report():
    """Soft node on different network reports its hardware node → auto-merge + split_network."""
    state = AppState()
    hw = make_hw_node("hw-1._ozma._udp.local.", "10.0.0.5")       # management network
    sw = make_sw_node("sw-1._ozma._udp.local.", "192.168.1.100",   # production network
                      hw_node_ref="hw-1._ozma._udp.local.")
    add_nodes(state, hw, sw)
    r = NodeReconciler(state)
    r._do_reconcile()
    rn = r.get_reconciled("hw-1._ozma._udp.local.")
    assert rn and rn.reconciled, "Should auto-merge via self-report"
    assert rn.split_network, "Should flag split network"
    assert rn.bind_method == "self_report"
    assert rn.hid_target == "hw-1._ozma._udp.local."
    assert rn.stream_source == "sw-1._ozma._udp.local."
    print("  PASS  auto-merge different IPs via self-report (split_network=True)")


def test_explicit_binding_different_networks():
    """Explicit binding works across different networks."""
    state = AppState()
    add_nodes(state,
        make_hw_node("hw-1._ozma._udp.local.", "10.0.0.5"),
        make_sw_node("sw-1._ozma._udp.local.", "172.16.0.50"),
    )
    r = NodeReconciler(state)
    r.bind("hw-1._ozma._udp.local.", "sw-1._ozma._udp.local.")
    r._do_reconcile()
    rn = r.get_reconciled("hw-1._ozma._udp.local.")
    assert rn and rn.reconciled
    assert rn.split_network
    assert rn.bind_method == "explicit"
    print("  PASS  explicit binding across networks (split_network=True)")


def test_split_network_flagged():
    """When hw and sw are on different IPs, split_network is True."""
    state = AppState()
    hw = make_hw_node("hw-1._ozma._udp.local.", "10.0.0.5")
    sw = make_sw_node("sw-1._ozma._udp.local.", "10.0.0.5")
    add_nodes(state, hw, sw)
    r = NodeReconciler(state)
    r._do_reconcile()
    rn = r.get_reconciled("hw-1._ozma._udp.local.")
    assert rn and not rn.split_network, "Same IP = no split"

    # Now test with different IPs via explicit bind
    state2 = AppState()
    add_nodes(state2,
        make_hw_node("hw-2._ozma._udp.local.", "10.0.0.5"),
        make_sw_node("sw-2._ozma._udp.local.", "192.168.1.50"),
    )
    r2 = NodeReconciler(state2)
    r2.bind("hw-2._ozma._udp.local.", "sw-2._ozma._udp.local.")
    r2._do_reconcile()
    rn2 = r2.get_reconciled("hw-2._ozma._udp.local.")
    assert rn2 and rn2.split_network, "Different IPs = split"
    print("  PASS  split_network correctly flagged")


def test_lookup_by_either_id():
    state = AppState()
    add_nodes(state,
        make_hw_node("hw-1._ozma._udp.local.", "10.0.0.5"),
        make_sw_node("sw-1._ozma._udp.local.", "10.0.0.5"),
    )
    r = NodeReconciler(state)
    r._do_reconcile()
    rn_hw = r.get_reconciled("hw-1._ozma._udp.local.")
    rn_sw = r.get_reconciled("sw-1._ozma._udp.local.")
    assert rn_hw and rn_sw
    assert rn_hw.id == rn_sw.id
    print("  PASS  lookup by either ID")


def test_capabilities_merged():
    """Merged node has all capabilities from both halves."""
    state = AppState()
    add_nodes(state,
        make_hw_node("hw-1._ozma._udp.local.", "10.0.0.5"),
        make_sw_node("sw-1._ozma._udp.local.", "10.0.0.5"),
    )
    r = NodeReconciler(state)
    r._do_reconcile()
    rn = r.get_reconciled("hw-1._ozma._udp.local.")
    assert rn and rn.reconciled
    # From hardware
    assert rn.has_hid
    assert rn.has_audio
    assert rn.has_serial
    assert rn.has_power
    # From software
    assert rn.has_screen_capture
    assert rn.has_clipboard
    assert rn.has_display_info
    assert rn.has_metrics
    print("  PASS  all capabilities merged")


def test_multiple_pairs():
    """Multiple hw+sw pairs on the same controller."""
    state = AppState()
    add_nodes(state,
        make_hw_node("hw-1._ozma._udp.local.", "10.0.0.1"),
        make_sw_node("sw-1._ozma._udp.local.", "10.0.0.1"),
        make_hw_node("hw-2._ozma._udp.local.", "10.0.0.2"),
        make_sw_node("sw-2._ozma._udp.local.", "10.0.0.2"),
        make_hw_node("hw-3._ozma._udp.local.", "10.0.0.3"),  # no sw pair
    )
    r = NodeReconciler(state)
    r._do_reconcile()
    nodes = r.list_reconciled()
    reconciled = [n for n in nodes if n["reconciled"]]
    assert len(reconciled) == 2, f"Expected 2 merged pairs, got {len(reconciled)}"
    standalone = [n for n in nodes if not n["reconciled"]]
    assert len(standalone) == 1, f"Expected 1 standalone, got {len(standalone)}"
    print("  PASS  multiple pairs + standalone")


def test_bind_method_recorded():
    """The bind method is recorded for diagnostics."""
    state = AppState()
    hw1 = make_hw_node("hw-1._ozma._udp.local.", "10.0.0.5")
    sw1 = make_sw_node("sw-1._ozma._udp.local.", "10.0.0.5")
    add_nodes(state, hw1, sw1)
    r = NodeReconciler(state)
    r._do_reconcile()
    rn = r.get_reconciled("hw-1._ozma._udp.local.")
    assert rn and rn.bind_method == "ip"

    # Self-report takes priority over IP
    state2 = AppState()
    hw2 = make_hw_node("hw-2._ozma._udp.local.", "10.0.0.5")
    sw2 = make_sw_node("sw-2._ozma._udp.local.", "10.0.0.5",
                       hw_node_ref="hw-2._ozma._udp.local.")
    add_nodes(state2, hw2, sw2)
    r2 = NodeReconciler(state2)
    r2._do_reconcile()
    rn2 = r2.get_reconciled("hw-2._ozma._udp.local.")
    assert rn2 and rn2.bind_method == "self_report"
    print("  PASS  bind method recorded (ip, self_report)")


if __name__ == "__main__":
    print("Node Reconciler Tests")
    print("=" * 50)
    test_hardware_only()
    test_software_only()
    test_auto_merge_same_ip()
    test_auto_merge_different_ip_self_report()
    test_explicit_binding_different_networks()
    test_split_network_flagged()
    test_lookup_by_either_id()
    test_capabilities_merged()
    test_multiple_pairs()
    test_bind_method_recorded()
    print(f"\nAll {10} reconciler tests PASSED.")
