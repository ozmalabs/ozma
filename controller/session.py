# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Session establishment — X25519 key exchange with identity binding.

Implements the session protocol from 17-security-architecture.md (v2):

  1. Controller sends: version + ctrl_eph_pub + signature + ctrl_cert
  2. Node verifies controller cert against mesh CA, verifies signature
  3. Node sends: version + node_eph_pub + signature + node_cert
  4. Controller verifies node cert against mesh CA, verifies signature
  5. Both compute DH shared secret → HKDF → session keys
  6. Both derive session ID from transcript, verify match

After session establishment, all traffic uses the SessionState from
transport.py (XChaCha20-Poly1305 AEAD).
"""

from __future__ import annotations

import asyncio
import json
import logging
import struct
import time
from dataclasses import dataclass
from typing import Any

from transport import (
    IdentityKeyPair, EphemeralKeyPair, SessionState,
    derive_session_keys, WIRE_VERSION,
)
from pairing import MeshCA, NodeCertificate

log = logging.getLogger("ozma.session")

SESSION_VERSION = 0x01
TIMESTAMP_TOLERANCE = 30.0  # seconds


@dataclass
class SessionInitMessage:
    """Session init sent by the controller to a node."""
    version: int
    ephemeral_pub: bytes        # 32 bytes X25519
    signature: bytes            # 64 bytes Ed25519
    controller_cert: NodeCertificate
    timestamp: float
    target_node_id: str

    def to_bytes(self) -> bytes:
        cert_json = json.dumps(self.controller_cert.to_dict()).encode()
        return (
            bytes([self.version])
            + self.ephemeral_pub
            + self.signature
            + struct.pack(">d", self.timestamp)
            + struct.pack(">H", len(self.target_node_id.encode()))
            + self.target_node_id.encode()
            + struct.pack(">H", len(cert_json))
            + cert_json
        )

    @classmethod
    def from_bytes(cls, data: bytes) -> "SessionInitMessage":
        version = data[0]
        eph_pub = data[1:33]
        sig = data[33:97]
        ts = struct.unpack(">d", data[97:105])[0]
        nid_len = struct.unpack(">H", data[105:107])[0]
        node_id = data[107:107+nid_len].decode()
        cert_len = struct.unpack(">H", data[107+nid_len:109+nid_len])[0]
        cert_json = json.loads(data[109+nid_len:109+nid_len+cert_len])
        cert = NodeCertificate.from_dict(cert_json)
        return cls(version=version, ephemeral_pub=eph_pub, signature=sig,
                   controller_cert=cert, timestamp=ts, target_node_id=node_id)


@dataclass
class SessionAcceptMessage:
    """Session accept sent by the node back to the controller."""
    version: int
    ephemeral_pub: bytes
    signature: bytes
    node_cert: NodeCertificate
    timestamp: float
    target_ctrl_id: str

    def to_bytes(self) -> bytes:
        cert_json = json.dumps(self.node_cert.to_dict()).encode()
        return (
            bytes([self.version])
            + self.ephemeral_pub
            + self.signature
            + struct.pack(">d", self.timestamp)
            + struct.pack(">H", len(self.target_ctrl_id.encode()))
            + self.target_ctrl_id.encode()
            + struct.pack(">H", len(cert_json))
            + cert_json
        )

    @classmethod
    def from_bytes(cls, data: bytes) -> "SessionAcceptMessage":
        version = data[0]
        eph_pub = data[1:33]
        sig = data[33:97]
        ts = struct.unpack(">d", data[97:105])[0]
        cid_len = struct.unpack(">H", data[105:107])[0]
        ctrl_id = data[107:107+cid_len].decode()
        cert_len = struct.unpack(">H", data[107+cid_len:109+cid_len])[0]
        cert_json = json.loads(data[109+cid_len:109+cid_len+cert_len])
        cert = NodeCertificate.from_dict(cert_json)
        return cls(version=version, ephemeral_pub=eph_pub, signature=sig,
                   node_cert=cert, timestamp=ts, target_ctrl_id=ctrl_id)


class SessionManager:
    """
    Manages encrypted sessions with all nodes.

    Handles session establishment (DH key exchange) and provides
    SessionState objects for encrypting/decrypting packets.
    """

    def __init__(self, mesh_ca: MeshCA) -> None:
        self._ca = mesh_ca
        self._sessions: dict[str, SessionState] = {}
        self._pending: dict[str, EphemeralKeyPair] = {}  # node_id → our ephemeral

    def get_session(self, node_id: str) -> SessionState | None:
        """Get the active session for a node, if one exists."""
        session = self._sessions.get(node_id)
        if session:
            return session
        return None

    def has_session(self, node_id: str) -> bool:
        return node_id in self._sessions

    # ── Controller-side session init ────────────────────────────────────────

    def create_session_init(self, node_id: str) -> bytes | None:
        """
        Create a SessionInit message for a node.

        Called by the controller to initiate a session.
        Returns serialised message bytes, or None if not ready.
        """
        ctrl_kp = self._ca.controller_keypair
        ctrl_cert = self._ca.controller_cert
        if not ctrl_kp or not ctrl_cert:
            return None

        # Generate ephemeral X25519 keypair for this session
        eph = EphemeralKeyPair.generate()
        self._pending[node_id] = eph

        ts = time.time()

        # Sign: eph_pub || node_id || timestamp || "controller"
        sign_payload = (
            eph.public_key
            + node_id.encode()
            + struct.pack(">d", ts)
            + b"controller"
        )
        sig = ctrl_kp.sign(sign_payload)

        msg = SessionInitMessage(
            version=SESSION_VERSION,
            ephemeral_pub=eph.public_key,
            signature=sig,
            controller_cert=ctrl_cert,
            timestamp=ts,
            target_node_id=node_id,
        )
        return msg.to_bytes()

    def complete_session(self, node_id: str, accept_bytes: bytes) -> SessionState | None:
        """
        Process a SessionAccept from a node and establish the session.

        Returns the SessionState if successful, None on failure.
        """
        eph = self._pending.pop(node_id, None)
        if not eph:
            log.warning("No pending session for %s", node_id)
            return None

        try:
            accept = SessionAcceptMessage.from_bytes(accept_bytes)
        except Exception as e:
            log.warning("Invalid session accept from %s: %s", node_id, e)
            return None

        # Verify timestamp
        if abs(time.time() - accept.timestamp) > TIMESTAMP_TOLERANCE:
            log.warning("Session accept from %s has stale timestamp", node_id)
            return None

        # Verify node certificate against mesh CA
        ca_pub = self._ca.ca_public_key
        if not ca_pub or not accept.node_cert.verify(ca_pub):
            log.warning("Invalid node certificate from %s", node_id)
            return None

        if accept.node_cert.is_expired():
            log.warning("Expired node certificate from %s", node_id)
            return None

        # Verify the node's signature on its ephemeral key
        verify_payload = (
            accept.ephemeral_pub
            + "controller".encode()  # target = controller
            + struct.pack(">d", accept.timestamp)
            + b"node"
        )
        if not IdentityKeyPair.verify(verify_payload, accept.signature,
                                       accept.node_cert.public_key):
            log.warning("Invalid session signature from %s", node_id)
            return None

        # Compute DH shared secret
        dh_secret = eph.dh(accept.ephemeral_pub)

        # Derive session keys
        session = SessionState.from_dh(
            dh_secret, eph.public_key, accept.ephemeral_pub,
            "controller", node_id,
        )
        self._sessions[node_id] = session

        log.info("Session established with %s (ID: %s)",
                 node_id, session.session_id.hex()[:16])
        return session

    # ── Node-side session handling ──────────────────────────────────────────
    # (Used by soft nodes and desktop nodes)

    @staticmethod
    def handle_session_init(
        init_bytes: bytes,
        node_keypair: IdentityKeyPair,
        node_cert: NodeCertificate,
        ca_public_key: bytes,
    ) -> tuple[bytes, SessionState] | None:
        """
        Process a SessionInit from the controller (node-side).

        Returns (accept_bytes, session_state) or None on failure.
        """
        try:
            init = SessionInitMessage.from_bytes(init_bytes)
        except Exception:
            return None

        # Verify timestamp
        if abs(time.time() - init.timestamp) > TIMESTAMP_TOLERANCE:
            return None

        # Verify controller certificate
        if not init.controller_cert.verify(ca_public_key):
            return None
        if init.controller_cert.is_expired():
            return None

        # Verify controller's signature on its ephemeral key
        verify_payload = (
            init.ephemeral_pub
            + init.target_node_id.encode()
            + struct.pack(">d", init.timestamp)
            + b"controller"
        )
        if not IdentityKeyPair.verify(verify_payload, init.signature,
                                       init.controller_cert.public_key):
            return None

        # Generate our ephemeral keypair
        eph = EphemeralKeyPair.generate()
        ts = time.time()

        # Sign our ephemeral key
        sign_payload = (
            eph.public_key
            + "controller".encode()
            + struct.pack(">d", ts)
            + b"node"
        )
        sig = node_keypair.sign(sign_payload)

        accept = SessionAcceptMessage(
            version=SESSION_VERSION,
            ephemeral_pub=eph.public_key,
            signature=sig,
            node_cert=node_cert,
            timestamp=ts,
            target_ctrl_id="controller",
        )

        # Compute DH and derive session
        # Keys are derived in controller perspective (ctrl-to-node, node-to-ctrl).
        # The node swaps send/recv: what the controller calls send, the node uses as recv.
        dh_secret = eph.dh(init.ephemeral_pub)
        ctrl_to_node, node_to_ctrl, nonce_seed, session_id = derive_session_keys(
            dh_secret, init.ephemeral_pub, eph.public_key,
            "controller", node_cert.node_id,
        )
        session = SessionState(
            node_id=node_cert.node_id,
            send_key=node_to_ctrl,   # node sends with node-to-ctrl key
            recv_key=ctrl_to_node,   # node receives with ctrl-to-node key
            nonce_seed=nonce_seed,
            session_id=session_id,
        )

        return accept.to_bytes(), session

    # ── Status ──────────────────────────────────────────────────────────────

    def list_sessions(self) -> list[dict]:
        return [
            {
                "node_id": s.node_id,
                "session_id": s.session_id.hex()[:16],
                "packets_sent": s.send_counter,
            }
            for s in self._sessions.values()
        ]

    def drop_session(self, node_id: str) -> None:
        """Drop a session (node went offline, etc.)."""
        self._sessions.pop(node_id, None)
        self._pending.pop(node_id, None)
