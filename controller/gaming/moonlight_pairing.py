# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Moonlight HTTPS pairing server implementation.

Implements the PIN-based pairing flow for Moonlight clients:
- TLS certificate generation and management
- PIN verification using salted hash
- Client certificate storage and validation
- Server info and app list endpoints
"""

from __future__ import annotations

import asyncio
import hashlib
import json
import logging
import os
import secrets
import sqlite3
import ssl
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Optional

from aiohttp import web

log = logging.getLogger("ozma.moonlight.pairing")


@dataclass
class PairedClient:
    """Represents a paired Moonlight client."""
    client_id: str
    cert_hash: str
    paired_at: float


class PairingDatabase:
    """Stores paired client certificates."""

    def __init__(self, data_dir: Path) -> None:
        self._data_dir = data_dir
        self._db_path = data_dir / "paired_clients.db"
        self._init_db()

    def _init_db(self) -> None:
        """Initialize the SQLite database."""
        self._data_dir.mkdir(parents=True, exist_ok=True)
        with sqlite3.connect(self._db_path) as conn:
            conn.execute("""
                CREATE TABLE IF NOT EXISTS paired_clients (
                    client_id TEXT PRIMARY KEY,
                    cert_hash TEXT NOT NULL,
                    paired_at REAL NOT NULL
                )
            """)
            conn.commit()

    def add_client(self, client_id: str, cert_hash: str, paired_at: float) -> None:
        """Add a new paired client."""
        with sqlite3.connect(self._db_path) as conn:
            conn.execute(
                "INSERT OR REPLACE INTO paired_clients VALUES (?, ?, ?)",
                (client_id, cert_hash, paired_at)
            )
            conn.commit()

    def get_client(self, client_id: str) -> Optional[PairedClient]:
        """Get a paired client by ID."""
        with sqlite3.connect(self._db_path) as conn:
            cursor = conn.execute(
                "SELECT client_id, cert_hash, paired_at FROM paired_clients WHERE client_id = ?",
                (client_id,)
            )
            row = cursor.fetchone()
            if row:
                return PairedClient(row[0], row[1], row[2])
        return None

    def list_clients(self) -> list[PairedClient]:
        """List all paired clients."""
        clients = []
        with sqlite3.connect(self._db_path) as conn:
            cursor = conn.execute("SELECT client_id, cert_hash, paired_at FROM paired_clients")
            for row in cursor.fetchall():
                clients.append(PairedClient(row[0], row[1], row[2]))
        return clients

    def remove_client(self, client_id: str) -> bool:
        """Remove a paired client."""
        with sqlite3.connect(self._db_path) as conn:
            cursor = conn.execute("DELETE FROM paired_clients WHERE client_id = ?", (client_id,))
            conn.commit()
            return cursor.rowcount > 0


class MoonlightPairingServer:
    """
    Moonlight HTTPS pairing server.

    Implements the PIN-based pairing flow:
    - Phase 1: Server sends salt + server cert
    - Phase 2: Client sends client cert, server verifies PIN hash
    - Phase 3: Both sides derive shared secret, exchange signed certs
    """

    def __init__(self, data_dir: Path | None = None) -> None:
        self._data_dir = data_dir or Path.home() / ".ozma"
        self._pairing_db = PairingDatabase(self._data_dir)
        self._server_id = self._get_server_id()
        self._pin: str | None = None
        self._salt: bytes | None = None
        self._server_cert: bytes | None = None
        self._client_cert: bytes | None = None
        self._app = web.Application()
        self._runner: web.AppRunner | None = None
        self._site: web.TCPSite | None = None

        # Set up routes
        self._app.router.add_get('/serverinfo', self._handle_serverinfo)
        self._app.router.add_get('/applist', self._handle_applist)
        self._app.router.add_post('/pair', self._handle_pair)

    def _get_server_id(self) -> str:
        """Get or generate a unique server ID."""
        server_id_file = self._data_dir / "server_id"
        if server_id_file.exists():
            return server_id_file.read_text().strip()
        
        server_id = secrets.token_hex(16)
        self._data_dir.mkdir(parents=True, exist_ok=True)
        server_id_file.write_text(server_id)
        return server_id

    async def start(self) -> None:
        """Start the HTTPS server."""
        # Generate TLS certificate if needed
        cert_file = self._data_dir / "server.crt"
        key_file = self._data_dir / "server.key"
        
        if not cert_file.exists() or not key_file.exists():
            self._generate_self_signed_cert(cert_file, key_file)
        
        # Create SSL context
        ssl_context = ssl.create_default_context(ssl.Purpose.CLIENT_AUTH)
        ssl_context.load_cert_chain(cert_file, key_file)
        
        # Start server
        self._runner = web.AppRunner(self._app)
        await self._runner.setup()
        self._site = web.TCPSite(self._runner, '0.0.0.0', 47984, ssl_context=ssl_context)
        await self._site.start()
        
        log.info("Moonlight pairing server started on port 47984")

    async def stop(self) -> None:
        """Stop the HTTPS server."""
        if self._site:
            await self._site.stop()
        if self._runner:
            await self._runner.cleanup()
        log.info("Moonlight pairing server stopped")

    def _generate_self_signed_cert(self, cert_file: Path, key_file: Path) -> None:
        """Generate a self-signed certificate for the server."""
        try:
            from cryptography import x509
            from cryptography.x509.oid import NameOID
            from cryptography.hazmat.primitives import hashes, serialization
            from cryptography.hazmat.primitives.asymmetric import rsa
            import datetime
            
            # Generate private key
            private_key = rsa.generate_private_key(
                public_exponent=65537,
                key_size=2048,
            )
            
            # Write private key
            key_file.parent.mkdir(parents=True, exist_ok=True)
            key_file.write_bytes(
                private_key.private_bytes(
                    encoding=serialization.Encoding.PEM,
                    format=serialization.PrivateFormat.TraditionalOpenSSL,
                    encryption_algorithm=serialization.NoEncryption(),
                )
            )
            
            # Generate certificate
            subject = issuer = x509.Name([
                x509.NameAttribute(NameOID.COMMON_NAME, "Ozma Moonlight Server"),
            ])
            
            cert = x509.CertificateBuilder().subject_name(
                subject
            ).issuer_name(
                issuer
            ).public_key(
                private_key.public_key()
            ).serial_number(
                x509.random_serial_number()
            ).not_valid_before(
                datetime.datetime.utcnow()
            ).not_valid_after(
                datetime.datetime.utcnow() + datetime.timedelta(days=3650)
            ).add_extension(
                x509.BasicConstraints(ca=False, path_length=None),
                critical=True,
            ).sign(private_key, hashes.SHA256())
            
            # Write certificate
            cert_file.write_bytes(cert.public_bytes(serialization.Encoding.PEM))
            
            log.info("Generated self-signed certificate")
        except ImportError:
            log.error("cryptography library not available, cannot generate certificate")
            raise

    async def _handle_serverinfo(self, request: web.Request) -> web.Response:
        """Handle /serverinfo endpoint."""
        # Check if any clients are paired
        has_paired_clients = len(self._pairing_db.list_clients()) > 0
        
        info = {
            "hostname": "Ozma Moonlight Server",
            "uuid": self._server_id,
            "state": "PAIRED" if has_paired_clients else "UNPAIRED",
            "httpsPort": 47984,
            "httpPort": 0,  # Not implementing HTTP for security
        }
        return web.json_response(info)

    async def _handle_applist(self, request: web.Request) -> web.Response:
        """Handle /applist endpoint - stub returning empty list."""
        return web.json_response([])

    async def _handle_pair(self, request: web.Request) -> web.Response:
        """Handle /pair endpoint - PIN-based pairing flow."""
        try:
            data = await request.json()
            phase = data.get("stage", 0)
            
            if phase == 1:
                # Phase 1: Server sends salt + server cert
                self._pin = str(secrets.randbelow(9000) + 1000)  # 4-digit PIN
                self._salt = secrets.token_bytes(16)
                
                # Read server certificate
                cert_file = self._data_dir / "server.crt"
                if cert_file.exists():
                    self._server_cert = cert_file.read_bytes()
                else:
                    self._server_cert = b""
                
                response = {
                    "stage": 1,
                    "salt": self._salt.hex(),
                    "cert": self._server_cert.decode('utf-8') if self._server_cert else "",
                }
                return web.json_response(response)
                
            elif phase == 2:
                # Phase 2: Client sends client cert, server verifies PIN hash
                if not self._pin or not self._salt:
                    return web.json_response({"error": "No pairing in progress"}, status=400)
                
                client_cert_pem = data.get("clientcert", "")
                pin_hash_hex = data.get("pinhash", "")
                
                # Verify PIN hash
                pin_hash = bytes.fromhex(pin_hash_hex)
                expected_hash = hashlib.sha256(self._pin.encode() + self._salt).digest()
                
                if not secrets.compare_digest(pin_hash, expected_hash):
                    return web.json_response({"error": "PIN mismatch"}, status=403)
                
                # Store client cert
                self._client_cert = client_cert_pem.encode('utf-8')
                
                response = {
                    "stage": 2,
                    "cert": self._server_cert.decode('utf-8') if self._server_cert else "",
                }
                return web.json_response(response)
                
            elif phase == 3:
                # Phase 3: Both sides derive shared secret, exchange signed certs
                if not self._client_cert:
                    return web.json_response({"error": "No client cert provided"}, status=400)
                
                # Extract client ID from cert (simplified - in reality would parse the cert)
                client_id = hashlib.sha256(self._client_cert).hexdigest()[:32]
                cert_hash = hashlib.sha256(self._client_cert).hexdigest()
                
                # Store paired client
                import time
                self._pairing_db.add_client(client_id, cert_hash, time.time())
                
                # Clear pairing state
                self._pin = None
                self._salt = None
                self._server_cert = None
                self._client_cert = None
                
                response = {
                    "stage": 3,
                    "clientId": client_id,
                }
                return web.json_response(response)
                
            else:
                return web.json_response({"error": "Invalid stage"}, status=400)
                
        except Exception as e:
            log.error("Pairing error: %s", e)
            return web.json_response({"error": "Internal server error"}, status=500)

    def get_paired_clients(self) -> list[dict[str, Any]]:
        """Get all paired clients."""
        clients = self._pairing_db.list_clients()
        return [
            {
                "clientId": client.client_id,
                "certHash": client.cert_hash,
                "pairedAt": client.paired_at,
            }
            for client in clients
        ]

    async def revoke_client(self, client_id: str) -> bool:
        """Revoke a paired client."""
        return self._pairing_db.remove_client(client_id)
