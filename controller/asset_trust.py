# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Trust store and signature verification for display assets.

Connect signs the canonical JSON of an index without its ``signature``
envelope.  Packs sign the canonical manifest JSON without the top-level
``signature`` field and without ``checksums.signature``.  This module verifies
those exact payloads against a deployment-supplied trust store, so the
controller never needs to import Connect or hold a signing secret.

The store is fail-closed: unknown, revoked, not-yet-valid, expired, or
algorithm-mismatched keys are rejected, as are malformed keys and signatures.
No key material is emitted by ``repr`` or error messages.

Base64 and hex key material are both accepted.  A bare string is interpreted
as hex when it is an even-length hex digest and as base64 otherwise; explicit
``hex:`` and ``base64:`` prefixes remove the ambiguity.

Ed25519 is the production algorithm.  HMAC-SHA256 exists for tests and
private deployments only.
"""

from __future__ import annotations

import base64
import binascii
import hashlib
import hmac as hmac_module
import json
from collections.abc import Iterable, Mapping, Sequence
from dataclasses import dataclass, field
from datetime import datetime, timezone
from typing import Any, Callable

try:  # pragma: no cover - availability depends on the deployment
    from cryptography.exceptions import InvalidSignature
    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey

    HAVE_ED25519 = True
except ImportError:  # pragma: no cover
    InvalidSignature = Exception  # type: ignore[assignment,misc]
    Ed25519PublicKey = None  # type: ignore[assignment]
    HAVE_ED25519 = False

__all__ = [
    "ALGORITHMS",
    "ED25519",
    "ENV_VAR",
    "HAVE_ED25519",
    "HMAC_SHA256",
    "INDEX_SCHEMA",
    "KeyVerifier",
    "MANIFEST_SCHEMA",
    "TrustError",
    "TrustStore",
    "TrustedKey",
    "build_verifier",
    "canonical_index_payload",
    "canonical_json",
    "canonical_manifest_payload",
    "key_fingerprint",
    "sha256_hex",
]

ED25519 = "ed25519"
HMAC_SHA256 = "hmac-sha256"
ALGORITHMS = frozenset({ED25519, HMAC_SHA256})

INDEX_SCHEMA = "org.ozma.asset-index/1"
MANIFEST_SCHEMA = "org.ozma.asset-pack/1"

ENV_VAR = "OZMA_DISPLAY_TRUST_KEYS"

_SIGNATURE_KEY = "signature"
_CHECKSUMS_KEY = "checksums"
_CHECKSUM_FILES_KEY = "files"


class TrustError(Exception):
    """Raised when a trust store or signature cannot be used safely."""


# ── Canonical payloads ──────────────────────────────────────────────────────


def canonical_json(value: Any) -> bytes:
    """Return deterministic canonical JSON bytes (Connect parity)."""
    return json.dumps(
        value,
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
        allow_nan=False,
    ).encode("utf-8")


def canonical_index_payload(index: Mapping[str, Any]) -> bytes:
    """Canonical index payload: the document without its signature envelope."""
    unsigned = {
        key: value for key, value in index.items() if key != _SIGNATURE_KEY
    }
    return canonical_json(unsigned)


def canonical_manifest_payload(manifest: Mapping[str, Any]) -> bytes:
    """Canonical manifest payload used for pack signatures.

    The top-level ``signature`` field and ``checksums.signature`` are removed;
    every other field (including ``checksums.files``) is signed.
    """
    unsigned = {
        key: value for key, value in manifest.items() if key != _SIGNATURE_KEY
    }
    checksums = unsigned.get(_CHECKSUMS_KEY)
    if isinstance(checksums, Mapping) and _SIGNATURE_KEY in checksums:
        cleaned = {
            key: value
            for key, value in checksums.items()
            if key != _SIGNATURE_KEY
        }
        unsigned[_CHECKSUMS_KEY] = cleaned
    return canonical_json(unsigned)


def sha256_hex(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


# ── Key material ────────────────────────────────────────────────────────────


def _b64decode(value: str) -> bytes:
    padding = "=" * (-len(value) % 4)
    return base64.b64decode(value + padding, validate=True)


def _is_hex(value: str) -> bool:
    return bool(value) and len(value) % 2 == 0 and all(
        character in "0123456789abcdefABCDEF" for character in value
    )


def _decode_material(value: str, *, label: str = "key material") -> bytes:
    text = str(value or "").strip()
    if not text:
        raise TrustError(f"{label} is empty")
    lowered = text.lower()
    try:
        if lowered.startswith("base64:"):
            raw = _b64decode(text[len("base64:"):].strip())
        elif lowered.startswith("hex:"):
            raw = bytes.fromhex(text[len("hex:"):].strip())
        elif _is_hex(text):
            raw = bytes.fromhex(text)
        else:
            raw = _b64decode(text)
    except (binascii.Error, ValueError, TypeError):
        raise TrustError(f"{label} is not valid base64 or hex") from None
    if not raw:
        raise TrustError(f"{label} is empty")
    return raw


def _decode_signature(value: Any, *, hex_only: bool = False) -> bytes | None:
    text = str(value or "").strip()
    if not text:
        return None
    try:
        if not hex_only and not _is_hex(text):
            return _b64decode(text)
        if _is_hex(text):
            return bytes.fromhex(text)
        if hex_only:
            return None
        return _b64decode(text)
    except (binascii.Error, ValueError, TypeError):
        return None


def _parse_timestamp(value: Any, *, label: str) -> datetime | None:
    if value is None:
        return None
    if isinstance(value, datetime):
        moment = value
    else:
        text = str(value).strip()
        if not text:
            return None
        if text.endswith(("Z", "z")):
            text = text[:-1] + "+00:00"
        try:
            moment = datetime.fromisoformat(text)
        except ValueError:
            raise TrustError(f"{label} is not an ISO timestamp") from None
    if moment.tzinfo is None:
        moment = moment.replace(tzinfo=timezone.utc)
    return moment.astimezone(timezone.utc)


def _normalise_algorithm(value: Any) -> str:
    algorithm = str(value or "").strip().lower()
    if algorithm == "hmac":
        algorithm = HMAC_SHA256
    if algorithm not in ALGORITHMS:
        raise TrustError(f"unsupported signature algorithm: {algorithm or '<empty>'!r}")
    return algorithm


def _as_bool(value: Any) -> bool:
    if isinstance(value, bool):
        return value
    if value is None:
        return False
    return str(value).strip().lower() not in {"", "0", "false", "no", "off"}


@dataclass(frozen=True)
class TrustedKey:
    """One deployment-trusted signing key.

    ``public_key`` holds raw Ed25519 public bytes (base64 or hex) or the shared
    HMAC secret for test/private deployments.  It is excluded from ``repr``.
    """

    key_id: str
    algorithm: str
    public_key: str = field(repr=False)
    revoked: bool = False
    not_before: str | datetime | None = None
    not_after: str | datetime | None = None
    comment: str = ""

    def __post_init__(self) -> None:
        key_id = str(self.key_id or "").strip()
        if not key_id:
            raise TrustError("key_id must not be empty")
        algorithm = _normalise_algorithm(self.algorithm)
        raw = _decode_material(self.public_key, label=f"public_key for {key_id!r}")
        if algorithm == ED25519 and len(raw) != 32:
            raise TrustError(
                f"ed25519 public key for {key_id!r} must be 32 bytes"
            )
        not_before = _parse_timestamp(self.not_before, label="not_before")
        not_after = _parse_timestamp(self.not_after, label="not_after")
        if not_before is not None and not_after is not None and not_before > not_after:
            raise TrustError(f"not_before is after not_after for key {key_id!r}")
        object.__setattr__(self, "key_id", key_id)
        object.__setattr__(self, "algorithm", algorithm)
        object.__setattr__(self, "revoked", _as_bool(self.revoked))
        object.__setattr__(
            self, "not_before", not_before.isoformat() if not_before else None
        )
        object.__setattr__(
            self, "not_after", not_after.isoformat() if not_after else None
        )
        object.__setattr__(self, "comment", str(self.comment or ""))

    # Material -------------------------------------------------------------

    def key_bytes(self) -> bytes:
        return _decode_material(
            self.public_key, label=f"public_key for {self.key_id!r}"
        )

    def fingerprint(self) -> str:
        return f"{self.algorithm}:{sha256_hex(self.key_bytes())}"

    # Validity -------------------------------------------------------------

    def is_active(self, now: datetime | None = None) -> bool:
        if self.revoked:
            return False
        moment = now or datetime.now(timezone.utc)
        if moment.tzinfo is None:
            moment = moment.replace(tzinfo=timezone.utc)
        moment = moment.astimezone(timezone.utc)
        if self.not_before is not None:
            if moment < _parse_timestamp(self.not_before, label="not_before"):
                return False
        if self.not_after is not None:
            if moment > _parse_timestamp(self.not_after, label="not_after"):
                return False
        return True

    def require_active(self, now: datetime | None = None) -> None:
        if self.revoked:
            raise TrustError(f"signing key {self.key_id!r} is revoked")
        if not self.is_active(now):
            raise TrustError(
                f"signing key {self.key_id!r} is not valid at this time"
            )


def key_fingerprint(key: TrustedKey) -> str:
    """Return a stable, non-secret fingerprint for a trusted key."""
    if not isinstance(key, TrustedKey):
        raise TrustError("key_fingerprint requires a TrustedKey")
    return key.fingerprint()


# ── Verification primitives ─────────────────────────────────────────────────


def _verify_signature(
    key: TrustedKey, payload: bytes, signature: Any, *, now: datetime | None = None
) -> bool:
    """Verify ``signature`` over ``payload`` with ``key``; never raises."""
    try:
        if not isinstance(payload, (bytes, bytearray, memoryview)):
            return False
        if not key.is_active(now):
            return False
        body = bytes(payload)
        if key.algorithm == ED25519:
            if not HAVE_ED25519 or Ed25519PublicKey is None:
                return False
            raw_signature = _decode_signature(signature)
            if raw_signature is None or len(raw_signature) != 64:
                return False
            public_key = Ed25519PublicKey.from_public_bytes(key.key_bytes())
            public_key.verify(raw_signature, body)
            return True
        if key.algorithm == HMAC_SHA256:
            expected_hex = hmac_module.new(
                key.key_bytes(), body, hashlib.sha256
            ).hexdigest()
            text = str(signature or "").strip()
            if len(text) == 64 and _is_hex(text):
                return hmac_module.compare_digest(expected_hex, text.lower())
            raw_signature = _decode_signature(signature)
            if raw_signature is None or len(raw_signature) != 32:
                return False
            expected = bytes.fromhex(expected_hex)
            return hmac_module.compare_digest(expected, raw_signature)
        return False
    except (InvalidSignature, ValueError, TypeError, TrustError, OSError):
        return False
    except Exception:  # pragma: no cover - defensive fail-closed
        return False


class KeyVerifier:
    """Connect-compatible verifier bound to one trusted key."""

    def __init__(self, key: TrustedKey, *, now: datetime | None = None) -> None:
        key.require_active(now)
        self._key = key
        self.algorithm = key.algorithm
        self.key_id = key.key_id

    def verify(self, payload: bytes, signature: str) -> bool:
        return _verify_signature(self._key, payload, signature)

    def __repr__(self) -> str:
        return f"KeyVerifier(key_id={self.key_id!r}, algorithm={self.algorithm!r})"


# ── Manifest checksums ──────────────────────────────────────────────────────


def _safe_relative_name(name: str) -> bool:
    text = str(name or "").strip()
    if not text or "\\" in text or text.startswith("/"):
        return False
    parts = text.split("/")
    return all(part not in {"", ".", ".."} for part in parts)


def _normalise_digest(value: Any) -> str:
    digest = str(value or "").strip().lower()
    if len(digest) != 64 or any(character not in "0123456789abcdef" for character in digest):
        raise TrustError("asset checksum is not a SHA-256 hex digest")
    return digest


def _declared_checksums(manifest: Mapping[str, Any]) -> dict[str, str]:
    declared: dict[str, str] = {}

    def add(name: Any, digest: Any) -> None:
        label = str(name or "").strip()
        if not _safe_relative_name(label):
            raise TrustError(f"unsafe asset path in manifest: {label!r}")
        normalised = _normalise_digest(digest)
        existing = declared.get(label)
        if existing is not None and existing != normalised:
            raise TrustError(f"conflicting checksums for {label!r}")
        declared[label] = normalised

    checksums = manifest.get(_CHECKSUMS_KEY)
    if isinstance(checksums, list):
        for item in checksums:
            if not isinstance(item, Mapping):
                raise TrustError("manifest checksum entries must be objects")
            name = item.get("path")
            digest = item.get("sha256") or item.get("hash")
            if name is None or digest is None:
                raise TrustError("manifest checksum entry is missing path or hash")
            add(name, digest)
    elif checksums is not None:
        if not isinstance(checksums, Mapping):
            raise TrustError("manifest checksums must be an object or list")
        algorithm = checksums.get("algorithm")
        if algorithm is not None and str(algorithm).strip().lower() != "sha256":
            raise TrustError("only sha256 asset checksums are supported")
        files = checksums.get(_CHECKSUM_FILES_KEY)
        if files is not None:
            if not isinstance(files, Mapping):
                raise TrustError("manifest checksums.files must be an object")
            for name, digest in files.items():
                add(name, digest)
        else:
            # Older packs used a flat ``checksums`` object.
            for name, digest in checksums.items():
                if str(name) in {"algorithm", "schema", _SIGNATURE_KEY}:
                    continue
                if isinstance(digest, Mapping):
                    digest = digest.get("sha256") or digest.get("hash")
                if digest is None:
                    raise TrustError(f"checksum for {name!r} is missing a hash")
                add(name, digest)

    assets = manifest.get("assets")
    if assets is not None:
        if not isinstance(assets, list):
            raise TrustError("manifest assets must be a list")
        for asset in assets:
            if not isinstance(asset, Mapping):
                raise TrustError("manifest asset entries must be objects")
            name = asset.get("path")
            digest = asset.get("sha256") or asset.get("hash")
            if name is None or digest is None:
                continue
            add(name, digest)

    return declared


def _manifest_hashes_ok(
    manifest: Mapping[str, Any], files: Mapping[str, Any]
) -> bool:
    try:
        declared = _declared_checksums(manifest)
    except TrustError:
        return False
    if not declared:
        return True
    if not isinstance(files, Mapping):
        return False
    for name, digest in declared.items():
        data = files.get(name)
        if isinstance(data, (bytes, bytearray, memoryview)):
            actual = sha256_hex(bytes(data))
        elif isinstance(data, str):
            if len(data) == 64 and _is_hex(data):
                actual = data.lower()
            else:
                actual = sha256_hex(data.encode("utf-8"))
        else:
            return False
        if not hmac_module.compare_digest(actual, digest):
            return False
    return True


# ── Trust store ─────────────────────────────────────────────────────────────


class TrustStore:
    """A fail-closed collection of trusted signing keys."""

    def __init__(
        self,
        keys: Iterable[TrustedKey | Mapping[str, Any]] = (),
        *,
        default_key_id: str | None = None,
    ) -> None:
        self._keys: dict[str, TrustedKey] = {}
        self._default_key_id = str(default_key_id).strip() if default_key_id else None
        for key in keys:
            self.add(key)
        if self._default_key_id is not None and self._default_key_id not in self._keys:
            raise TrustError(
                f"default key {self._default_key_id!r} is not in the trust store"
            )

    # Construction ---------------------------------------------------------

    @classmethod
    def from_mapping(
        cls, raw: Mapping[str, Any] | Sequence[Any] | None
    ) -> TrustStore:
        """Build a store from JSON-shaped data.

        Accepted shapes::

            {"default_key_id": "prod", "keys": [ {...}, {...} ]}
            [ {"key_id": "prod", ...}, ... ]
            {"prod": {"algorithm": "ed25519", "public_key": "..."}, ...}
            {"key_id": "prod", "algorithm": "ed25519", "public_key": "..."}
        """
        if isinstance(raw, TrustStore):
            return cls(raw, default_key_id=raw.default_key_id)
        if raw is None:
            raise TrustError("trust store is missing")
        default_key_id: str | None = None
        entries: list[Any] = []

        if isinstance(raw, Mapping):
            if "keys" in raw:
                default_key_id = _default_from_mapping(raw)
                entries = list(_as_entry_list(raw["keys"]))
            elif _looks_like_single_key(raw):
                entries = [raw]
                if _as_bool(raw.get("default")) and raw.get("key_id"):
                    default_key_id = str(raw["key_id"])
            else:
                default_key_id = _default_from_mapping(raw)
                for key_id, spec in raw.items():
                    if str(key_id) in {"default_key_id", "default"}:
                        continue
                    if not isinstance(spec, Mapping):
                        raise TrustError(
                            f"key {key_id!r} must be an object"
                        )
                    entries.append({**spec, "key_id": key_id})
        elif isinstance(raw, (list, tuple)):
            entries = list(raw)
        else:
            raise TrustError("trust store must be a JSON object or list")

        keys: list[TrustedKey] = []
        for entry in entries:
            keys.append(_key_from_mapping(entry))
        return cls(keys, default_key_id=default_key_id)

    @classmethod
    def from_env(
        cls,
        environ: Mapping[str, str] | None = None,
        *,
        variable: str = ENV_VAR,
    ) -> TrustStore:
        """Load the trust store from the JSON environment variable."""
        source = environ
        if source is None:
            import os

            source = os.environ
        raw = str(source.get(variable) or "").strip()
        if not raw:
            raise TrustError(f"{variable} is not set")
        try:
            payload = json.loads(raw)
        except ValueError:
            raise TrustError(f"{variable} is not valid JSON") from None
        return cls.from_mapping(payload)

    # Access ---------------------------------------------------------------

    @property
    def default_key_id(self) -> str | None:
        return self._default_key_id

    @property
    def keys(self) -> tuple[TrustedKey, ...]:
        return tuple(self._keys.values())

    def __len__(self) -> int:
        return len(self._keys)

    def __iter__(self):
        return iter(self._keys.values())

    def __repr__(self) -> str:
        return (
            f"TrustStore(keys={sorted(self._keys)!r}, "
            f"default_key_id={self._default_key_id!r})"
        )

    def add(self, key: TrustedKey | Mapping[str, Any]) -> TrustStore:
        trusted = key if isinstance(key, TrustedKey) else _key_from_mapping(key)
        if trusted.key_id in self._keys:
            raise TrustError(f"duplicate key_id: {trusted.key_id!r}")
        self._keys[trusted.key_id] = trusted
        return self

    def get(self, key_id: str) -> TrustedKey | None:
        return self._keys.get(str(key_id or "").strip())

    def require(self, key_id: str) -> TrustedKey:
        key = self.get(key_id)
        if key is None:
            raise TrustError(f"unknown signing key: {key_id!r}")
        return key

    def fingerprint(self, key_id: str | None = None) -> str:
        if key_id is None:
            chosen = self._default_key_id
            if chosen is None:
                if len(self._keys) != 1:
                    raise TrustError("a key_id is required for this trust store")
                chosen = next(iter(self._keys))
            key_id = chosen
        return self.require(key_id).fingerprint()

    # Verification ---------------------------------------------------------

    def verifier_for(self, key_id: str | None = None) -> KeyVerifier:
        if key_id is None:
            if self._default_key_id is None:
                raise TrustError("a key_id is required for this trust store")
            key_id = self._default_key_id
        return KeyVerifier(self.require(key_id))

    def _signature_for(
        self, document: Mapping[str, Any], *, now: datetime | None = None
    ) -> tuple[TrustedKey, str, str]:
        signature = document.get(_SIGNATURE_KEY)
        if signature is None:
            checksums = document.get(_CHECKSUMS_KEY)
            if isinstance(checksums, Mapping):
                signature = checksums.get(_SIGNATURE_KEY)
        if isinstance(signature, Mapping):
            algorithm = _normalise_algorithm(signature.get("algorithm"))
            key_id = str(signature.get("key_id") or "").strip()
            value = signature.get("value")
            if not key_id:
                raise TrustError("signature envelope is missing key_id")
            if not isinstance(value, str) or not value:
                raise TrustError("signature envelope is missing its value")
            key = self.require(key_id)
            key.require_active(now)
            return key, algorithm, value
        if isinstance(signature, str):
            value = signature.strip()
            if not value:
                raise TrustError("signature is empty")
            if self._default_key_id is None:
                raise TrustError(
                    "bare signatures require a configured default key"
                )
            key = self.require(self._default_key_id)
            key.require_active(now)
            return key, key.algorithm, value
        raise TrustError("signature is missing or malformed")

    def verify_index(
        self,
        index: Mapping[str, Any],
        *,
        now: datetime | None = None,
    ) -> bool:
        """Verify a Connect-style index: hash, ETag, and signature."""
        try:
            if not isinstance(index, Mapping):
                return False
            entries = index.get("entries")
            if not isinstance(entries, list):
                return False
            expected_hash = sha256_hex(canonical_json(entries))
            content_hash = str(index.get("content_hash") or "")
            if not content_hash or not hmac_module.compare_digest(
                content_hash, expected_hash
            ):
                return False
            etag = index.get("etag")
            if etag is not None and str(etag) != f'"{expected_hash}"':
                return False
            key, algorithm, value = self._signature_for(index, now=now)
            if algorithm != key.algorithm:
                return False
            return _verify_signature(
                key, canonical_index_payload(index), value, now=now
            )
        except (TrustError, ValueError, TypeError, KeyError):
            return False
        except Exception:  # pragma: no cover - defensive fail-closed
            return False

    def verify_manifest(
        self,
        manifest: Mapping[str, Any],
        files: Mapping[str, bytes] | None = None,
        *,
        now: datetime | None = None,
    ) -> bool:
        """Verify a pack manifest signature and, optionally, declared hashes."""
        try:
            if not isinstance(manifest, Mapping):
                return False
            key, algorithm, value = self._signature_for(manifest, now=now)
            if algorithm != key.algorithm:
                return False
            if not _verify_signature(
                key, canonical_manifest_payload(manifest), value, now=now
            ):
                return False
            if files is not None and not _manifest_hashes_ok(manifest, files):
                return False
            return True
        except (TrustError, ValueError, TypeError, KeyError):
            return False
        except Exception:  # pragma: no cover - defensive fail-closed
            return False


def _as_entry_list(value: Any) -> Iterable[Any]:
    if isinstance(value, Mapping):
        for key_id, spec in value.items():
            if not isinstance(spec, Mapping):
                raise TrustError(f"key {key_id!r} must be an object")
            yield {**spec, "key_id": key_id}
        return
    if isinstance(value, (list, tuple)):
        yield from value
        return
    raise TrustError("trust store keys must be a list or object")


def _looks_like_single_key(raw: Mapping[str, Any]) -> bool:
    return "key_id" in raw and ("public_key" in raw or "key" in raw)


def _default_from_mapping(raw: Mapping[str, Any]) -> str | None:
    value = raw.get("default_key_id", raw.get("default"))
    if isinstance(value, bool):
        return None
    text = str(value or "").strip()
    return text or None


def _key_from_mapping(raw: Any) -> TrustedKey:
    if isinstance(raw, TrustedKey):
        return raw
    if not isinstance(raw, Mapping):
        raise TrustError("trusted key entry must be an object")
    key_id = raw.get("key_id", raw.get("id"))
    algorithm = raw.get("algorithm", raw.get("alg"))
    public_key = raw.get("public_key", raw.get("key"))
    if key_id is None or algorithm is None or public_key is None:
        raise TrustError("trusted key entry requires key_id, algorithm, public_key")
    return TrustedKey(
        key_id=str(key_id),
        algorithm=str(algorithm),
        public_key=str(public_key),
        revoked=_as_bool(raw.get("revoked")),
        not_before=raw.get("not_before", raw.get("notBefore")),
        not_after=raw.get("not_after", raw.get("notAfter")),
        comment=str(raw.get("comment") or ""),
    )


# ── Repository adapter ──────────────────────────────────────────────────────


def _looks_like_index(document: Mapping[str, Any]) -> bool:
    return str(document.get("schema") or "") == INDEX_SCHEMA or (
        isinstance(document.get("entries"), list)
        and "content_hash" in document
    )


class _RepositoryVerifier:
    """``(payload, signature, files) -> bool`` adapter for AssetRepository."""

    def __init__(self, store: TrustStore, key: TrustedKey) -> None:
        self._store = store
        self._key = key

    def __call__(
        self,
        payload: bytes,
        signature: str,
        files: Mapping[str, bytes] | None = None,
    ) -> bool:
        try:
            if not isinstance(payload, (bytes, bytearray, memoryview)):
                return False
            document = json.loads(bytes(payload).decode("utf-8"))
            if not isinstance(document, Mapping):
                return False
            if _looks_like_index(document):
                canonical = canonical_index_payload(document)
                return _verify_signature(self._key, canonical, signature)
            canonical = canonical_manifest_payload(document)
            if not _verify_signature(self._key, canonical, signature):
                return False
            if files is not None and not _manifest_hashes_ok(document, files):
                return False
            return True
        except (ValueError, TypeError, UnicodeError, TrustError):
            return False
        except Exception:  # pragma: no cover - defensive fail-closed
            return False

    def __repr__(self) -> str:
        return (
            f"_RepositoryVerifier(key_id={self._key.key_id!r}, "
            f"algorithm={self._key.algorithm!r})"
        )


def build_verifier(
    trust_store: TrustStore | Mapping[str, Any] | Sequence[Any],
) -> Callable[[bytes, str, Mapping[str, bytes] | None], bool]:
    """Return the callable expected by ``AssetRepository``.

    The key is unambiguous when the store has an explicit default or exactly
    one usable key.  Ambiguity is a configuration error and raises
    :class:`TrustError` rather than silently accepting or rejecting packs.
    """
    store = (
        trust_store
        if isinstance(trust_store, TrustStore)
        else TrustStore.from_mapping(trust_store)
    )
    key: TrustedKey | None = None
    if store.default_key_id is not None:
        key = store.require(store.default_key_id)
        key.require_active()
    else:
        usable = [item for item in store.keys if item.is_active()]
        if len(usable) == 1:
            key = usable[0]
        elif not usable:
            raise TrustError("trust store has no usable signing key")
        else:
            raise TrustError(
                "trust store has multiple signing keys; configure a default key"
            )
    return _RepositoryVerifier(store, key)
