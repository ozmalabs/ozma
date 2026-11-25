"""Environment-driven bootstrap for the Connect shared asset database.

The asset router resolves ``app.state.asset_service`` at request time.  This
module turns deployment environment variables into that service without making
the rest of Connect depend on Postgres, object storage, or signing keys.

Configuration is deliberately boring and fail-closed:

* ``OZMA_CONNECT_ASSET_DB`` (or the legacy ``OZMA_CONNECT_DATABASE_URL``) is a
  SQLAlchemy URL.  When it is unset the router keeps returning ``503``.
* ``OZMA_CONNECT_ASSET_STORAGE`` is a filesystem directory for content-addressed
  payloads.  It defaults to ``./connect-assets``.
* ``OZMA_CONNECT_ASSET_SIGNING_KEY`` is an optional Ed25519 private key encoded
  as raw base64, raw hex, or PEM.  HMAC keys are intentionally unsupported in
  the deployment path.
* ``OZMA_CONNECT_ASSET_INDEX_CHANNEL`` records the deployment's default index
  channel on ``app.state``.

Errors are raised as :class:`BootstrapError` with operator-safe messages.  The
original URL or key material is never included in an error string or log line.
"""

from __future__ import annotations

import base64
import os
from pathlib import Path
from typing import Any, Mapping

from sqlalchemy import create_engine
from sqlalchemy.engine import make_url
from sqlalchemy.pool import StaticPool

from connect.assets.index import (
    HAVE_ED25519,
    AssetService,
    Ed25519Signer,
    Signer,
)
from connect.assets.models import create_schema
from connect.assets.storage import FileStore, ObjectStore

DEFAULT_STORAGE_DIR = "./connect-assets"
DEFAULT_INDEX_CHANNEL = "stable"
DEFAULT_SIGNING_KEY_ID = "connect"

DATABASE_ENV = "OZMA_CONNECT_ASSET_DB"
LEGACY_DATABASE_ENV = "OZMA_CONNECT_DATABASE_URL"
STORAGE_ENV = "OZMA_CONNECT_ASSET_STORAGE"
SIGNING_KEY_ENV = "OZMA_CONNECT_ASSET_SIGNING_KEY"
SIGNING_KEY_ID_ENV = "OZMA_CONNECT_ASSET_SIGNING_KEY_ID"
INDEX_CHANNEL_ENV = "OZMA_CONNECT_ASSET_INDEX_CHANNEL"

_HEX_DIGITS = frozenset("0123456789abcdefABCDEF")


class BootstrapError(RuntimeError):
    """A deployment configuration error with a safe, machine-readable code."""

    def __init__(
        self,
        message: str,
        *,
        code: str = "asset_bootstrap_error",
    ) -> None:
        super().__init__(message)
        self.code = code


def _value(environ: Mapping[str, str], name: str) -> str:
    raw = environ.get(name)
    if raw is None:
        return ""
    return str(raw).strip()


def _database_url(environ: Mapping[str, str]) -> str:
    return _value(environ, DATABASE_ENV) or _value(
        environ, LEGACY_DATABASE_ENV
    )


def _index_channel(environ: Mapping[str, str]) -> str:
    return _value(environ, INDEX_CHANNEL_ENV) or DEFAULT_INDEX_CHANNEL


def _build_engine(url: str) -> Any:
    """Create a SQLAlchemy engine without leaking the DSN in exceptions."""
    try:
        parsed = make_url(url)
        backend = parsed.get_backend_name()
    except Exception:
        raise BootstrapError(
            "OZMA_CONNECT_ASSET_DB is not a valid SQLAlchemy URL",
            code="invalid_database_url",
        ) from None

    options: dict[str, Any] = {}
    if backend == "sqlite":
        options["connect_args"] = {"check_same_thread": False}
        if not parsed.database or parsed.database == ":memory:":
            # In-memory SQLite must share one connection across request threads.
            options["poolclass"] = StaticPool
    else:
        options["pool_pre_ping"] = True

    try:
        return create_engine(url, **options)
    except Exception:
        raise BootstrapError(
            "OZMA_CONNECT_ASSET_DB could not create a database engine",
            code="invalid_database_url",
        ) from None


def _ensure_schema(engine: Any) -> None:
    try:
        create_schema(engine)
    except Exception:
        raise BootstrapError(
            "asset database schema could not be initialised",
            code="database_unavailable",
        ) from None


def _build_store(environ: Mapping[str, str]) -> FileStore:
    directory = _value(environ, STORAGE_ENV) or DEFAULT_STORAGE_DIR
    target = Path(directory).expanduser()
    created = not target.exists()
    try:
        target.mkdir(parents=True, exist_ok=True, mode=0o700)
        if created:
            # mkdir mode is filtered through the umask; make the intent explicit.
            try:
                os.chmod(target, 0o700)
            except OSError:
                pass
        return FileStore(target)
    except Exception:
        raise BootstrapError(
            "asset storage directory could not be prepared",
            code="invalid_storage",
        ) from None


def _b64decode(value: str) -> bytes:
    padded = value + "=" * (-len(value) % 4)
    return base64.b64decode(padded, validate=True)


def _decode_key_material(value: str) -> bytes:
    text = value.strip()
    if not text:
        raise ValueError("empty key")
    lowered = text.lower()
    if lowered.startswith("hex:"):
        return bytes.fromhex(text[len("hex:"):].strip())
    if lowered.startswith("base64:"):
        return _b64decode(text[len("base64:"):].strip())
    compact = "".join(text.split())
    if (
        len(compact) >= 2
        and len(compact) % 2 == 0
        and all(character in _HEX_DIGITS for character in compact)
    ):
        return bytes.fromhex(compact)
    return _b64decode(compact)


def _load_ed25519_private_key(value: str):
    """Decode raw base64/hex or PEM key material into an Ed25519 key."""
    if not HAVE_ED25519:  # pragma: no cover - depends on optional dependency
        raise BootstrapError(
            "Ed25519 signing requires the 'cryptography' package",
            code="signing_unavailable",
        )

    try:
        from cryptography.hazmat.primitives import serialization
        from cryptography.hazmat.primitives.asymmetric.ed25519 import (
            Ed25519PrivateKey,
        )
    except ImportError:  # pragma: no cover - defensive import
        raise BootstrapError(
            "Ed25519 signing requires the 'cryptography' package",
            code="signing_unavailable",
        ) from None

    try:
        if "-----BEGIN" in value:
            key = serialization.load_pem_private_key(
                value.encode("utf-8"),
                password=None,
            )
        else:
            material = _decode_key_material(value)
            if len(material) == 32:
                key = Ed25519PrivateKey.from_private_bytes(material)
            else:
                key = serialization.load_der_private_key(
                    material,
                    password=None,
                )
        if not isinstance(key, Ed25519PrivateKey):
            raise ValueError("not an Ed25519 private key")
        return key
    except BootstrapError:
        raise
    except Exception:
        # Includes binascii, ASN.1, and unsupported-algorithm failures.  The
        # original exception is suppressed so no key material can reach a log.
        raise BootstrapError(
            "OZMA_CONNECT_ASSET_SIGNING_KEY is not a valid Ed25519 private key",
            code="invalid_signing_key",
        ) from None


def _build_signer(environ: Mapping[str, str]) -> Signer | None:
    value = _value(environ, SIGNING_KEY_ENV)
    if not value:
        return None
    key = _load_ed25519_private_key(value)
    key_id = _value(environ, SIGNING_KEY_ID_ENV) or DEFAULT_SIGNING_KEY_ID
    return Ed25519Signer(key, key_id=key_id)


def _dispose(engine: Any) -> None:
    try:
        engine.dispose()
    except Exception:  # pragma: no cover - best-effort cleanup
        pass


def configure_assets(
    app: Any,
    environ: Mapping[str, str] | None = None,
    *,
    engine: Any = None,
    store: ObjectStore | None = None,
    signer: Signer | None = None,
) -> AssetService | None:
    """Configure ``app.state.asset_service`` from deployment environment.

    Repeated calls are idempotent and return the already-configured service.
    Without an explicit ``engine``, an unset database URL leaves the router
    unavailable (``503``) and returns ``None``.  Invalid URLs, storage paths, and
    signing keys raise :class:`BootstrapError`; no secret is included in the
    message.
    """
    existing = getattr(app.state, "asset_service", None)
    if existing is not None:
        return existing

    env = os.environ if environ is None else environ
    channel = _index_channel(env)
    try:
        app.state.asset_index_channel = channel
    except Exception:  # pragma: no cover - unusual application objects
        pass

    created_engine = False
    if engine is None:
        url = _database_url(env)
        if not url:
            return None
        engine = _build_engine(url)
        created_engine = True

    try:
        _ensure_schema(engine)
        if store is None:
            store = _build_store(env)
        if signer is None:
            signer = _build_signer(env)
        service = AssetService(engine, store=store, signer=signer)
    except BootstrapError:
        if created_engine:
            _dispose(engine)
        raise
    except Exception:
        if created_engine:
            _dispose(engine)
        raise BootstrapError(
            "asset service could not be configured",
            code="bootstrap_failed",
        ) from None

    app.state.asset_service = service
    return service


__all__ = [
    "BootstrapError",
    "DATABASE_ENV",
    "DEFAULT_INDEX_CHANNEL",
    "DEFAULT_SIGNING_KEY_ID",
    "DEFAULT_STORAGE_DIR",
    "INDEX_CHANNEL_ENV",
    "LEGACY_DATABASE_ENV",
    "SIGNING_KEY_ENV",
    "SIGNING_KEY_ID_ENV",
    "STORAGE_ENV",
    "configure_assets",
]
