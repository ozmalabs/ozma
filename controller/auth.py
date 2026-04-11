# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
API authentication — JWT bearer tokens signed with the controller's Ed25519 key.

Implements the control plane auth model from docs/security.md:

  - Human/application clients authenticate with a bearer token (JWT)
  - JWTs are signed with the controller's Ed25519 identity key
  - WireGuard-sourced requests (10.200.x.x) bypass token requirements
  - Scopes: read, write, admin

On first run with no password configured, a random admin password is generated
and printed to the console.
"""

from __future__ import annotations

import base64
import hashlib
import hmac
import ipaddress
import json
import logging
import os
import secrets
import struct
import time
from dataclasses import dataclass, field

from transport import IdentityKeyPair

log = logging.getLogger("ozma.auth")

_HAS_NACL = False
try:
    import nacl.pwhash
    import nacl.utils
    _HAS_NACL = True
except ImportError:
    pass

# Scopes
SCOPE_READ = "read"
SCOPE_WRITE = "write"
SCOPE_ADMIN = "admin"
ALL_SCOPES = [SCOPE_READ, SCOPE_WRITE, SCOPE_ADMIN]


@dataclass
class AuthConfig:
    enabled: bool = True
    password_hash: str = ""                         # Argon2id hash (stored in config/env)
    jwt_expiry_seconds: int = 86400                 # 24 hours
    wireguard_bypass_subnets: list[str] = field(
        default_factory=lambda: ["10.200.0.0/16"]
    )

    def _parsed_subnets(self) -> list[ipaddress.IPv4Network | ipaddress.IPv6Network]:
        return [ipaddress.ip_network(s, strict=False) for s in self.wireguard_bypass_subnets]


@dataclass(frozen=True)
class AuthContext:
    """Injected into request state after auth middleware runs."""
    authenticated: bool
    scopes: list[str]
    source_ip: str
    auth_method: str   # "jwt", "wireguard", "none"
    user_id: str = ""  # UUID of the authenticated user (empty for legacy/wireguard)


# ── Password hashing ──────────────────────────────────────────────────────

def hash_password(password: str) -> str:
    """Hash a password with Argon2id. Returns a string safe for JSON/env storage."""
    if _HAS_NACL:
        hashed = nacl.pwhash.argon2id.str(password.encode())
        return hashed.decode("ascii")
    # Fallback: PBKDF2-SHA256 (always available)
    salt = os.urandom(16)
    dk = hashlib.pbkdf2_hmac("sha256", password.encode(), salt, 600_000)
    return f"pbkdf2:sha256:600000${base64.b64encode(salt).decode()}${base64.b64encode(dk).decode()}"


def verify_password(password: str, stored_hash: str) -> bool:
    """Verify a password against a stored hash."""
    if stored_hash.startswith("$argon2") and _HAS_NACL:
        try:
            return nacl.pwhash.argon2id.verify(stored_hash.encode(), password.encode())
        except Exception:
            return False
    if stored_hash.startswith("pbkdf2:"):
        _, algo, rest = stored_hash.split(":", 2)
        iterations_str, salt_b64, dk_b64 = rest.split("$", 2)
        salt = base64.b64decode(salt_b64)
        expected = base64.b64decode(dk_b64)
        dk = hashlib.pbkdf2_hmac(algo, password.encode(), salt, int(iterations_str))
        return hmac.compare_digest(dk, expected)
    return False


def generate_admin_password() -> str:
    """Generate a random admin password for first-run setup."""
    return secrets.token_urlsafe(16)


# ── JWT (Ed25519-signed) ──────────────────────────────────────────────────

def _b64url_encode(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode("ascii")


def _b64url_decode(s: str) -> bytes:
    padded = s + "=" * (4 - len(s) % 4)
    return base64.urlsafe_b64decode(padded)


def create_jwt(keypair: IdentityKeyPair, scopes: list[str],
               expiry_seconds: int = 86400, subject: str = "admin",
               audience: str | None = None,
               issuer: str | None = None) -> str:
    """
    Create a JWT signed with Ed25519.

    Args:
        keypair: The Ed25519 keypair for signing
        scopes: List of scopes (read, write, admin)
        expiry_seconds: Token expiry time in seconds
        subject: The subject of the token (user ID or "admin")
        audience: Optional audience claim (aud)
        issuer: Optional issuer claim (iss)

    Returns:
        The signed JWT token string
    """
    header = {"alg": "EdDSA", "typ": "JWT"}
    now = int(time.time())
    payload = {
        "sub": subject,
        "scopes": scopes,
        "iat": now,
        "exp": now + expiry_seconds,
    }
    # Add optional claims
    if audience is not None:
        payload["aud"] = audience
    if issuer is not None:
        payload["iss"] = issuer
    header_b64 = _b64url_encode(json.dumps(header, separators=(",", ":")).encode())
    payload_b64 = _b64url_encode(json.dumps(payload, separators=(",", ":")).encode())
    signing_input = f"{header_b64}.{payload_b64}".encode()
    signature = keypair.sign(signing_input)
    sig_b64 = _b64url_encode(signature)
    return f"{header_b64}.{payload_b64}.{sig_b64}"


def verify_jwt(token: str, public_key: bytes,
               expected_audience: str | None = None,
               expected_issuer: str | None = None) -> dict | None:
    """
    Verify a JWT and return its claims, or None if invalid.

    Checks: signature (Ed25519), expiry, required fields, and optionally
    audience and issuer claims if provided.

    Args:
        token: The JWT token string
        public_key: The Ed25519 public key for signature verification
        expected_audience: Optional expected audience claim (aud)
        expected_issuer: Optional expected issuer claim (iss)

    Returns:
        The payload dict if valid, None if invalid
    """
    parts = token.split(".")
    if len(parts) != 3:
        return None
    header_b64, payload_b64, sig_b64 = parts
    try:
        header = json.loads(_b64url_decode(header_b64))
        if header.get("alg") != "EdDSA":
            return None
        signing_input = f"{header_b64}.{payload_b64}".encode()
        signature = _b64url_decode(sig_b64)
        if not IdentityKeyPair.verify(signing_input, signature, public_key):
            return None
        payload = json.loads(_b64url_decode(payload_b64))
        if payload.get("exp", 0) < time.time():
            return None
        if "sub" not in payload or "scopes" not in payload:
            return None

        # Validate audience if expected
        if expected_audience is not None:
            payload_audience = payload.get("aud")
            if isinstance(payload_audience, list):
                if expected_audience not in payload_audience:
                    return None
            elif payload_audience != expected_audience:
                return None

        # Validate issuer if expected
        if expected_issuer is not None:
            payload_issuer = payload.get("iss")
            if payload_issuer != expected_issuer:
                return None

        return payload
    except Exception:
        return None


# ── Network checks ────────────────────────────────────────────────────────

def is_wireguard_source(client_ip: str, config: AuthConfig) -> bool:
    """Check if a request originates from within a WireGuard bypass subnet."""
    try:
        addr = ipaddress.ip_address(client_ip)
        return any(addr in net for net in config._parsed_subnets())
    except ValueError:
        return False


# ── Scope checking ────────────────────────────────────────────────────────

def has_scope(ctx: AuthContext, required: str) -> bool:
    """Check if the auth context has the required scope. Admin implies all."""
    if SCOPE_ADMIN in ctx.scopes:
        return True
    return required in ctx.scopes


# ── First-run setup ──────────────────────────────────────────────────────

def setup_auth_password(env_password: str | None = None) -> tuple[str, str]:
    """
    Set up the admin password. Returns (password_hash, plaintext_password).

    If OZMA_AUTH_PASSWORD is set, uses that. Otherwise generates a random one.
    The plaintext is returned so main.py can print it on first run.
    """
    if env_password:
        return hash_password(env_password), env_password
    password = generate_admin_password()
    return hash_password(password), password
