# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for pairing.py — MeshCA, certificate issuance, revocation."""
import hashlib
import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).parent.parent.parent.parent / "controller"))
pytestmark = pytest.mark.unit


@pytest.fixture
def mesh_ca(tmp_path):
    from pairing import MeshCA
    ca = MeshCA(registry_path=tmp_path / "mesh_registry.json")
    ca.initialise()
    return ca


def _seed(node_id: str) -> bytes:
    return hashlib.sha256(node_id.encode()).digest()


class TestMeshCA:
    def test_initialise_creates_ca_keypair(self, mesh_ca):
        assert mesh_ca.ca_public_key is not None
        assert len(mesh_ca.ca_public_key) == 32

    def test_initialise_creates_controller_keypair(self, mesh_ca):
        assert mesh_ca.controller_keypair is not None
        assert len(mesh_ca.controller_keypair.public_key) == 32

    def test_status_has_fingerprints(self, mesh_ca):
        status = mesh_ca.status()
        assert status.get("ca_fingerprint")
        assert status.get("controller_fingerprint")

    def test_initialise_is_idempotent(self, tmp_path):
        from pairing import MeshCA
        path = tmp_path / "mesh_registry.json"
        ca1 = MeshCA(registry_path=path)
        ca1.initialise()
        pubkey1 = ca1.ca_public_key

        ca2 = MeshCA(registry_path=path)
        ca2.initialise()
        # Reloading the same file must produce the same key
        assert ca2.ca_public_key == pubkey1


class TestNodeApproval:
    def test_approve_node_returns_cert(self, mesh_ca):
        node_id = "test-node._ozma._udp.local."
        cert = mesh_ca.approve_node(node_id, _seed(node_id), ["hid"])
        assert cert is not None

    def test_is_node_trusted_after_approve(self, mesh_ca):
        node_id = "trusted._ozma._udp.local."
        mesh_ca.approve_node(node_id, _seed(node_id), ["hid"])
        assert mesh_ca.is_node_trusted(node_id)

    def test_unknown_node_not_trusted(self, mesh_ca):
        assert not mesh_ca.is_node_trusted("stranger._ozma._udp.local.")

    def test_approve_multiple_nodes(self, mesh_ca):
        for i in range(3):
            nid = f"node-{i}._ozma._udp.local."
            mesh_ca.approve_node(nid, _seed(nid), ["hid"])
        nodes = mesh_ca.list_nodes()
        assert len(nodes) == 3

    def test_list_nodes_empty_initially(self, mesh_ca):
        assert mesh_ca.list_nodes() == []

    def test_node_cert_pubkey_retrievable(self, mesh_ca):
        node_id = "with-pubkey._ozma._udp.local."
        mesh_ca.approve_node(node_id, _seed(node_id), ["hid"])
        pubkey = mesh_ca.get_node_pubkey(node_id)
        assert pubkey is not None
        assert len(pubkey) == 32


class TestRevocation:
    def test_revoke_node(self, mesh_ca):
        node_id = "revoke-me._ozma._udp.local."
        mesh_ca.approve_node(node_id, _seed(node_id), ["hid"])
        assert mesh_ca.is_node_trusted(node_id)
        mesh_ca.revoke_node(node_id, "test revocation")
        assert not mesh_ca.is_node_trusted(node_id)

    def test_revoked_node_absent_from_list(self, mesh_ca):
        nid = "gone._ozma._udp.local."
        mesh_ca.approve_node(nid, _seed(nid), ["hid"])
        mesh_ca.revoke_node(nid)
        ids = [n["node_id"] for n in mesh_ca.list_nodes()]
        assert nid not in ids


class TestPairingChallenge:
    def test_create_challenge_returns_two_values(self, mesh_ca):
        nonce, payload = mesh_ca.create_challenge()
        assert isinstance(nonce, bytes)
        assert isinstance(payload, bytes)
        assert len(nonce) == 32
        assert len(payload) > 32   # nonce + CA key + controller cert

    def test_verify_pairing_response(self, mesh_ca):
        from transport import IdentityKeyPair
        nonce, _ = mesh_ca.create_challenge()
        node_kp = IdentityKeyPair.generate()
        # Node signs: nonce + node_pubkey + "I am a node"
        proof = node_kp.sign(nonce + node_kp.public_key + b"I am a node")
        assert mesh_ca.verify_pairing_response(nonce, node_kp.public_key, proof)

    def test_verify_pairing_wrong_proof(self, mesh_ca):
        from transport import IdentityKeyPair
        nonce, _ = mesh_ca.create_challenge()
        node_kp = IdentityKeyPair.generate()
        bad_proof = b"\x00" * 64
        assert not mesh_ca.verify_pairing_response(nonce, node_kp.public_key, bad_proof)
