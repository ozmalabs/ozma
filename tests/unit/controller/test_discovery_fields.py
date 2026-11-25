# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""mDNS TXT parsing for seat identity and the VM guest-IP alias.

Covers the controller side of the Phase 4 registration-parity fix: Rust nodes
emit ``seat_id`` and both ``vm_ip``/``vm_guest_ip``, while Python soft nodes emit
only ``vm_ip``. No live zeroconf browser is involved.
"""

from __future__ import annotations

import pytest

from config import Config
from discovery import DiscoveryService, _seat_id_from_txt, _vm_guest_ip_from_txt
from state import AppState

pytestmark = pytest.mark.unit


class _FakeServiceInfo:
    def __init__(self, service_type: str, name: str, properties: dict[str, str]):
        self.type = service_type
        self.name = name
        self.port = 7331
        self.properties = {
            key.encode(): value.encode() for key, value in properties.items()
        }

    async def async_request(self, _zeroconf, timeout: int = 3000) -> None:
        del timeout

    def parsed_addresses(self) -> list[str]:
        return ["10.0.0.50"]


async def _resolve(monkeypatch, name: str, properties: dict[str, str]):
    monkeypatch.setattr(
        "discovery.AsyncServiceInfo",
        lambda service_type, service_name: _FakeServiceInfo(
            service_type, service_name, properties
        ),
    )
    state = AppState()
    service = DiscoveryService(Config(), state)
    await service._resolve_and_add(None, "_ozma._udp.local.", name)
    return state.nodes[name]


class TestTxtFieldHelpers:
    def test_seat_id_is_trimmed_and_absent_for_old_nodes(self):
        assert _seat_id_from_txt({"seat_id": " rig-1-seat-0 "}) == "rig-1-seat-0"
        assert _seat_id_from_txt({}) == ""

    def test_vm_ip_prefers_the_python_mdns_key_then_the_direct_alias(self):
        assert _vm_guest_ip_from_txt({"vm_ip": "10.0.0.42"}) == "10.0.0.42"
        assert _vm_guest_ip_from_txt({"vm_guest_ip": "10.0.0.43"}) == "10.0.0.43"
        assert (
            _vm_guest_ip_from_txt(
                {"vm_ip": "10.0.0.42", "vm_guest_ip": "10.0.0.43"}
            )
            == "10.0.0.42"
        )
        assert _vm_guest_ip_from_txt({}) is None


class TestDiscoveryRegistration:
    async def test_rust_node_registers_explicit_seat_and_guest_ip(self, monkeypatch):
        node = await _resolve(
            monkeypatch,
            "rig-1-seat-0._ozma._udp.local.",
            {
                "proto": "1",
                "role": "compute",
                "hw": "desktop-linux",
                "cap": "softnode,screen",
                "seat_id": "rig-1-seat-0",
                "vm_ip": "10.0.0.42",
                "vm_guest_ip": "10.0.0.42",
            },
        )

        assert node.seat_id == "rig-1-seat-0"
        assert node.vm_guest_ip == "10.0.0.42"
        assert node.host == "10.0.0.50"

    async def test_python_soft_node_still_parses_vm_ip_without_seat_id(self, monkeypatch):
        node = await _resolve(
            monkeypatch,
            "vm1._ozma._udp.local.",
            {
                "proto": "1",
                "role": "compute",
                "hw": "soft",
                "cap": "qmp,power",
                "vm_ip": "10.0.0.43",
            },
        )

        assert node.seat_id == ""
        assert node.vm_guest_ip == "10.0.0.43"

    async def test_non_compute_or_wrong_proto_is_ignored(self, monkeypatch):
        monkeypatch.setattr(
            "discovery.AsyncServiceInfo",
            lambda service_type, service_name: _FakeServiceInfo(
                service_type, service_name, {"proto": "2", "vm_ip": "10.0.0.44"}
            ),
        )
        state = AppState()
        service = DiscoveryService(Config(), state)
        await service._resolve_and_add(None, "_ozma._udp.local.", "bad._ozma._udp.local.")

        assert state.nodes == {}
