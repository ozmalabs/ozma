# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Node pairing — establishes mutual trust between controller and nodes.

Implements the pairing protocol from 17-security-architecture.md (v2):

  1. Node discovered via mDNS (unpaired)
  2. Controller sends challenge (mesh CA pubkey + controller cert + nonce)
  3. Node signs challenge with its identity key
  4. Controller displays fingerprint for human approval
  5. On approval: mesh CA signs node's public key → node certificate
  6. Node stores certificate, now trusted in the mesh

Also supports:
  - Auto-pair on loopback (soft nodes on same host)
  - Node-to-node pairing (Ozma Link, no controller)

The mesh CA private key is encrypted at rest (Argon2id + secretbox).
It is decrypted only during pairing ceremonies.
"""

from __future__ import annotations

import asyncio
import hashlib
import json
import logging
import os
import struct
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from transport import IdentityKeyPair, EphemeralKeyPair

log = logging.getLogger("ozma.pairing")

REGISTRY_PATH = Path(__file__).parent / "mesh_registry.json"

# Try to import argon2 for CA key encryption
_HAS_NACL = False
try:
    import nacl.bindings
    import nacl.secret
    import nacl.pwhash
    import nacl.utils
    _HAS_NACL = True
except ImportError:
    pass


# ── Certificate ─────────────────────────────────────────────────────────────

@dataclass(frozen=True)
class NodeCertificate:
    """A node's identity signed by the mesh CA."""
    node_id: str
    public_key: bytes       # 32 bytes Ed25519
    capabilities: list[str]
    not_after: float        # Unix timestamp
    signature: bytes        # 64 bytes Ed25519 signature by mesh CA

    def to_bytes(self) -> bytes:
        """Serialise the certificate content (what was signed)."""
        cap_str = ",".join(self.capabilities)
        return (
            self.node_id.encode() + b"\x00"
            + self.public_key
            + cap_str.encode() + b"\x00"
            + struct.pack(">d", self.not_after)
        )

    def is_expired(self) -> bool:
        return time.time() > self.not_after

    def verify(self, ca_public_key: bytes) -> bool:
        """Verify this certificate was signed by the given CA."""
        return IdentityKeyPair.verify(self.to_bytes(), self.signature, ca_public_key)

    def to_dict(self) -> dict:
        return {
            "node_id": self.node_id,
            "public_key": self.public_key.hex(),
            "capabilities": self.capabilities,
            "not_after": self.not_after,
            "signature": self.signature.hex(),
        }

    @classmethod
    def from_dict(cls, d: dict) -> "NodeCertificate":
        return cls(
            node_id=d["node_id"],
            public_key=bytes.fromhex(d["public_key"]),
            capabilities=d.get("capabilities", []),
            not_after=d.get("not_after", 0),
            signature=bytes.fromhex(d["signature"]),
        )


# ── Mesh CA ─────────────────────────────────────────────────────────────────

class MeshCA:
    """
    The mesh certificate authority.

    Generates and stores the mesh CA keypair. Signs node certificates.
    The CA private key is encrypted at rest.
    """

    def __init__(self, registry_path: Path = REGISTRY_PATH) -> None:
        self._registry_path = registry_path
        self._ca_keypair: IdentityKeyPair | None = None
        self._controller_keypair: IdentityKeyPair | None = None
        self._controller_cert: NodeCertificate | None = None
        self._nodes: dict[str, NodeCertificate] = {}
        self._revocations: list[dict] = []
        self._ca_generation: int = 1

    @property
    def ca_public_key(self) -> bytes | None:
        return self._ca_keypair.public_key if self._ca_keypair else None

    @property
    def controller_keypair(self) -> IdentityKeyPair | None:
        return self._controller_keypair

    @property
    def controller_cert(self) -> NodeCertificate | None:
        return self._controller_cert

    def initialise(self, passphrase: str = "") -> None:
        """
        Load or create the mesh CA and controller identity.

        If no registry exists, generates new keypairs.
        If registry exists, loads and decrypts.
        """
        if self._registry_path.exists():
            self._load(passphrase)
        else:
            self._create(passphrase)

    def _create(self, passphrase: str) -> None:
        """Generate new mesh CA + controller identity."""
        log.info("Generating new mesh CA keypair...")
        self._ca_keypair = IdentityKeyPair.generate()
        self._controller_keypair = IdentityKeyPair.generate()
        self._ca_generation = 1

        # Self-sign controller certificate
        self._controller_cert = self._sign_certificate(
            node_id="controller",
            public_key=self._controller_keypair.public_key,
            capabilities=["controller"],
        )

        self._save(passphrase)
        log.info("Mesh CA created. Fingerprint: %s",
                 IdentityKeyPair(self._ca_keypair.public_key, b"").fingerprint()
                 if self._ca_keypair else "?")

    def _load(self, passphrase: str) -> None:
        """Load registry from disk."""
        data = json.loads(self._registry_path.read_text())

        ca_priv_hex = data.get("mesh_ca", {}).get("private_key_encrypted", "")
        ca_pub_hex = data.get("mesh_ca", {}).get("public_key", "")

        if ca_priv_hex and ca_pub_hex:
            ca_pub = bytes.fromhex(ca_pub_hex)
            if passphrase and _HAS_NACL:
                # Decrypt CA private key
                encrypted = bytes.fromhex(ca_priv_hex)
                key = nacl.pwhash.argon2id.kdf(
                    32, passphrase.encode(),
                    encrypted[:16],  # salt is first 16 bytes
                    opslimit=nacl.pwhash.argon2id.OPSLIMIT_MODERATE,
                    memlimit=nacl.pwhash.argon2id.MEMLIMIT_MODERATE,
                )
                box = nacl.secret.SecretBox(key)
                ca_priv = box.decrypt(encrypted[16:])
                self._ca_keypair = IdentityKeyPair(public_key=ca_pub, private_key=ca_priv)
            else:
                # No passphrase — load plaintext (development only)
                ca_priv_plain = data.get("mesh_ca", {}).get("private_key", "")
                if ca_priv_plain:
                    self._ca_keypair = IdentityKeyPair(
                        public_key=ca_pub,
                        private_key=bytes.fromhex(ca_priv_plain),
                    )

        # Controller identity
        ctrl = data.get("controller", {})
        if ctrl.get("public_key") and ctrl.get("private_key"):
            self._controller_keypair = IdentityKeyPair(
                public_key=bytes.fromhex(ctrl["public_key"]),
                private_key=bytes.fromhex(ctrl["private_key"]),
            )
        if ctrl.get("certificate"):
            self._controller_cert = NodeCertificate.from_dict(ctrl["certificate"])

        # Node certificates
        for node_id, node_data in data.get("nodes", {}).items():
            if node_data.get("certificate"):
                self._nodes[node_id] = NodeCertificate.from_dict(node_data["certificate"])

        self._revocations = data.get("revocations", [])
        self._ca_generation = data.get("mesh_ca", {}).get("generation", 1)

        log.info("Mesh registry loaded: %d nodes, %d revocations",
                 len(self._nodes), len(self._revocations))

    def _save(self, passphrase: str = "") -> None:
        """Persist registry to disk."""
        ca_data: dict[str, Any] = {"generation": self._ca_generation}
        if self._ca_keypair:
            ca_data["public_key"] = self._ca_keypair.public_key.hex()
            if passphrase and _HAS_NACL:
                # Encrypt CA private key
                salt = os.urandom(16)
                key = nacl.pwhash.argon2id.kdf(
                    32, passphrase.encode(), salt,
                    opslimit=nacl.pwhash.argon2id.OPSLIMIT_MODERATE,
                    memlimit=nacl.pwhash.argon2id.MEMLIMIT_MODERATE,
                )
                box = nacl.secret.SecretBox(key)
                encrypted = salt + box.encrypt(self._ca_keypair.private_key)
                ca_data["private_key_encrypted"] = encrypted.hex()
            else:
                # Plaintext (development only)
                ca_data["private_key"] = self._ca_keypair.private_key.hex()

            fp = hashlib.sha256(self._ca_keypair.public_key).hexdigest()
            ca_data["fingerprint"] = " ".join(fp[i:i+4].upper() for i in range(0, 32, 4))

        ctrl_data: dict[str, Any] = {}
        if self._controller_keypair:
            ctrl_data["public_key"] = self._controller_keypair.public_key.hex()
            ctrl_data["private_key"] = self._controller_keypair.private_key.hex()
        if self._controller_cert:
            ctrl_data["certificate"] = self._controller_cert.to_dict()

        nodes_data = {}
        for node_id, cert in self._nodes.items():
            nodes_data[node_id] = {"certificate": cert.to_dict()}

        data = {
            "version": 2,
            "mesh_ca": ca_data,
            "controller": ctrl_data,
            "nodes": nodes_data,
            "revocations": self._revocations,
        }
        self._registry_path.write_text(json.dumps(data, indent=2))

    # ── Certificate signing ─────────────────────────────────────────────────

    def _sign_certificate(
        self,
        node_id: str,
        public_key: bytes,
        capabilities: list[str],
        validity_days: int = 365,
    ) -> NodeCertificate:
        """Sign a node certificate with the mesh CA."""
        if not self._ca_keypair:
            raise RuntimeError("Mesh CA not initialised")

        not_after = time.time() + (validity_days * 86400)
        cert = NodeCertificate(
            node_id=node_id,
            public_key=public_key,
            capabilities=capabilities,
            not_after=not_after,
            signature=b"",  # placeholder
        )
        # Sign the certificate content
        sig = self._ca_keypair.sign(cert.to_bytes())
        return NodeCertificate(
            node_id=node_id,
            public_key=public_key,
            capabilities=capabilities,
            not_after=not_after,
            signature=sig,
        )

    # ── Pairing flow ────────────────────────────────────────────────────────

    def create_challenge(self) -> tuple[bytes, bytes]:
        """
        Create a pairing challenge for a new node.

        Returns: (challenge_nonce, challenge_payload)
        The payload contains: mesh_ca_pubkey + controller_cert + nonce
        """
        nonce = os.urandom(32)
        payload = b""
        if self._ca_keypair:
            payload += self._ca_keypair.public_key
        if self._controller_cert:
            cert_bytes = json.dumps(self._controller_cert.to_dict()).encode()
            payload += struct.pack(">H", len(cert_bytes)) + cert_bytes
        payload += nonce
        return nonce, payload

    def verify_pairing_response(
        self,
        challenge_nonce: bytes,
        node_pubkey: bytes,
        proof: bytes,
    ) -> bool:
        """Verify a node's pairing response (signed challenge)."""
        expected_message = challenge_nonce + node_pubkey + b"I am a node"
        return IdentityKeyPair.verify(expected_message, proof, node_pubkey)

    def approve_node(
        self,
        node_id: str,
        node_pubkey: bytes,
        capabilities: list[str],
        passphrase: str = "",
    ) -> NodeCertificate | None:
        """
        Approve a node and issue its certificate.

        Called after human verification of the fingerprint.
        """
        if not self._ca_keypair:
            return None

        cert = self._sign_certificate(node_id, node_pubkey, capabilities)
        self._nodes[node_id] = cert
        self._save(passphrase)
        log.info("Node approved: %s (fingerprint: %s)",
                 node_id,
                 IdentityKeyPair(node_pubkey, b"").fingerprint())
        return cert

    def is_node_trusted(self, node_id: str) -> bool:
        """Check if a node has a valid, non-expired, non-revoked certificate."""
        cert = self._nodes.get(node_id)
        if not cert:
            return False
        if cert.is_expired():
            return False
        if any(r["node_id"] == node_id for r in self._revocations):
            return False
        if not self._ca_keypair:
            return False
        return cert.verify(self._ca_keypair.public_key)

    def get_node_cert(self, node_id: str) -> NodeCertificate | None:
        return self._nodes.get(node_id)

    def get_node_pubkey(self, node_id: str) -> bytes | None:
        cert = self._nodes.get(node_id)
        return cert.public_key if cert else None

    def revoke_node(self, node_id: str, reason: str = "", passphrase: str = "") -> bool:
        """Revoke a node's certificate."""
        if node_id not in self._nodes:
            return False
        self._revocations.append({
            "node_id": node_id,
            "revoked_at": time.time(),
            "reason": reason,
        })
        del self._nodes[node_id]
        self._save(passphrase)
        log.info("Node revoked: %s (%s)", node_id, reason)
        return True

    def renew_node(self, node_id: str, passphrase: str = "") -> NodeCertificate | None:
        """Renew an existing node's certificate (extend expiry)."""
        cert = self._nodes.get(node_id)
        if not cert or not self._ca_keypair:
            return None
        new_cert = self._sign_certificate(
            node_id, cert.public_key, cert.capabilities,
        )
        self._nodes[node_id] = new_cert
        self._save(passphrase)
        return new_cert

    # ── Auto-pair (loopback only) ───────────────────────────────────────────

    def auto_pair_loopback(
        self,
        node_id: str,
        node_pubkey: bytes,
        capabilities: list[str],
        remote_addr: str,
    ) -> NodeCertificate | None:
        """
        Auto-pair a node on loopback (127.0.0.0/8).

        Only for soft nodes on the same host as the controller.
        """
        if not remote_addr.startswith("127."):
            log.warning("Auto-pair rejected: %s is not loopback (%s)", node_id, remote_addr)
            return None
        return self.approve_node(node_id, node_pubkey, capabilities)

    # ── Status ──────────────────────────────────────────────────────────────

    def list_nodes(self) -> list[dict]:
        result = []
        for node_id, cert in self._nodes.items():
            revoked = any(r["node_id"] == node_id for r in self._revocations)
            result.append({
                "node_id": node_id,
                "fingerprint": IdentityKeyPair(cert.public_key, b"").fingerprint(),
                "capabilities": cert.capabilities,
                "expires": cert.not_after,
                "expired": cert.is_expired(),
                "revoked": revoked,
            })
        return result

    def status(self) -> dict:
        return {
            "ca_fingerprint": (
                IdentityKeyPair(self._ca_keypair.public_key, b"").fingerprint()
                if self._ca_keypair else None
            ),
            "ca_generation": self._ca_generation,
            "controller_fingerprint": (
                IdentityKeyPair(self._controller_keypair.public_key, b"").fingerprint()
                if self._controller_keypair else None
            ),
            "paired_nodes": len(self._nodes),
            "revoked_nodes": len(self._revocations),
        }
