"""Signed asset index generation and the shared asset service.

The index is canonical JSON: sorted keys, compact separators and UTF-8.  A
content hash is computed over the entry list only, so rebuilding an unchanged
database produces the same ETag even when ``generated_at`` changes.  Signature
payloads cover the whole unsigned index, including the ETag.

Signing is injectable.  When ``cryptography`` is installed, Ed25519 signers and
verifiers are available.  Tests may use the HMAC implementation instead.  No
key material is bundled with the application.
"""

from __future__ import annotations

import base64
import hashlib
import hmac
import json
import re
from dataclasses import dataclass
from datetime import datetime
from typing import Any, Mapping, Protocol

from sqlalchemy import select
from sqlalchemy.orm import Session, selectinload

from connect.assets.models import (
    ASSET_SCHEMA,
    DEFAULT_ALLOWED_LICENSES,
    INDEX_SCHEMA,
    PACK_TYPES,
    AssetConflictError,
    AssetError,
    AssetFile,
    AssetNotFoundError,
    AssetPack,
    AssetReview,
    AssetVersion,
    InvalidAssetError,
    apply_pack_summary,
    as_bool,
    as_mapping,
    as_str_list,
    json_dumps,
    license_is_allowed,
    manifest_summary,
    normalize_license,
    utc_now_iso,
    version_key,
)
from connect.assets.review import ReviewService
from connect.assets.storage import (
    ObjectStore,
    put_content,
    sha256_hex,
    validate_key,
)

_ID_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")
_MAX_INDEX_BYTES = 2 * 1024 * 1024
_MAX_PAGE_SIZE = 200
_EXECUTABLE_SANDBOXES = frozenset({"iframe", "wasm"})

_SIGNATURE_KEY = "signature"
_SIGNATURE_ALGORITHM = "ed25519"


def canonical_json(value: Any) -> bytes:
    """Return deterministic canonical JSON bytes."""
    return json.dumps(
        value,
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
        allow_nan=False,
    ).encode("utf-8")


# ── Signing ──────────────────────────────────────────────────────────────────


class Signer(Protocol):
    algorithm: str
    key_id: str

    def sign(self, payload: bytes) -> str:
        ...


class Verifier(Protocol):
    algorithm: str

    def verify(self, payload: bytes, signature: str) -> bool:
        ...


def _b64encode(value: bytes) -> str:
    return base64.b64encode(value).decode("ascii")


def _b64decode(value: str) -> bytes:
    padding = "=" * (-len(value) % 4)
    return base64.b64decode(value + padding, validate=True)


class HMACSigner:
    """HMAC-SHA256 signer for tests and private deployments.

    This is a typed fallback; production publishes Ed25519 keys.
    """

    algorithm = "hmac-sha256"

    def __init__(self, key: bytes, *, key_id: str = "test") -> None:
        if not key:
            raise ValueError("HMAC key must not be empty")
        self._key = bytes(key)
        self.key_id = key_id

    def sign(self, payload: bytes) -> str:
        return hmac.new(self._key, payload, hashlib.sha256).hexdigest()


class HMACVerifier:
    algorithm = "hmac-sha256"

    def __init__(self, key: bytes) -> None:
        if not key:
            raise ValueError("HMAC key must not be empty")
        self._key = bytes(key)

    def verify(self, payload: bytes, signature: str) -> bool:
        expected = hmac.new(self._key, payload, hashlib.sha256).hexdigest()
        return hmac.compare_digest(expected, str(signature or ""))


try:  # pragma: no cover - import availability differs by environment
    from cryptography.exceptions import InvalidSignature
    from cryptography.hazmat.primitives.asymmetric.ed25519 import (
        Ed25519PrivateKey,
        Ed25519PublicKey,
    )

    HAVE_ED25519 = True
except ImportError:  # pragma: no cover
    HAVE_ED25519 = False

if HAVE_ED25519:

    class Ed25519Signer:
        algorithm = _SIGNATURE_ALGORITHM

        def __init__(self, private_key: Any, *, key_id: str = "") -> None:
            if isinstance(private_key, (bytes, bytearray, memoryview)):
                private_key = Ed25519PrivateKey.from_private_bytes(
                    bytes(private_key)
                )
            self._private_key = private_key
            self.key_id = key_id

        def sign(self, payload: bytes) -> str:
            signature = self._private_key.sign(payload)
            return _b64encode(signature)

        def public_key_bytes(self) -> bytes:
            from cryptography.hazmat.primitives import serialization

            return self._private_key.public_key().public_bytes(
                encoding=serialization.Encoding.Raw,
                format=serialization.PublicFormat.Raw,
            )

    class Ed25519Verifier:
        algorithm = _SIGNATURE_ALGORITHM

        def __init__(self, public_key: Any) -> None:
            if isinstance(public_key, (bytes, bytearray, memoryview)):
                public_key = Ed25519PublicKey.from_public_bytes(
                    bytes(public_key)
                )
            self._public_key = public_key

        def verify(self, payload: bytes, signature: str) -> bool:
            try:
                self._public_key.verify(_b64decode(signature), payload)
            except (InvalidSignature, ValueError, TypeError):
                return False
            return True

else:  # pragma: no cover - exercised only without cryptography

    class Ed25519Signer:
        algorithm = _SIGNATURE_ALGORITHM

        def __init__(self, *_args: Any, **_kwargs: Any) -> None:
            raise ImportError(
                "Ed25519 signing requires the 'cryptography' package"
            )

    class Ed25519Verifier:
        algorithm = _SIGNATURE_ALGORITHM

        def __init__(self, *_args: Any, **_kwargs: Any) -> None:
            raise ImportError(
                "Ed25519 verification requires the 'cryptography' package"
            )


# ── Index generation ─────────────────────────────────────────────────────────


def _generated_at(value: str | datetime | None) -> str:
    if value is None:
        return utc_now_iso()
    if isinstance(value, datetime):
        return value.isoformat()
    return str(value)


def _declared_channel(manifest: Mapping[str, Any], pack: AssetPack) -> str:
    declared = manifest.get("channel")
    if not declared:
        declared = pack.extra().get("channel")
    return str(declared or "stable").strip().lower()


def _entry_payload(pack: AssetPack, version: AssetVersion) -> dict[str, Any]:
    """Merge stored manifest metadata with authoritative relational fields."""
    entry = version.manifest()
    entry.update({
        "id": pack.id,
        "type": pack.type,
        "name": pack.name,
        "version": version.version,
        "author": pack.author,
        "description": pack.description,
        "license": pack.license,
        "source_url": pack.source_url,
        "confidence": pack.confidence,
        "min_ozma_version": version.min_ozma_version,
        "min_display_schema": version.min_display_schema,
        "signature": version.signature,
        "published_at": version.published_at,
        "yanked": bool(version.yanked),
        "tags": [tag.tag for tag in pack.tags],
        "capabilities": pack.capabilities(),
        "permissions": pack.permissions(),
        "assets": [asset.to_dict() for asset in version.files],
    })
    if pack.provenance:
        stored_provenance = entry.get("provenance")
        merged_provenance = (
            dict(stored_provenance)
            if isinstance(stored_provenance, Mapping)
            else {}
        )
        merged_provenance.update(pack.provenance[0].to_dict())
        entry["provenance"] = merged_provenance
    entry.setdefault("schema", ASSET_SCHEMA)
    return entry


def _index_entry(pack: AssetPack, version: AssetVersion) -> dict[str, Any]:
    """Compact install metadata for the signed index.

    Unknown manifest fields stay in the database and in the entry endpoint;
    the index intentionally carries only the fields a controller needs to
    search, verify, and install a pack.  Empty optional fields are omitted to
    keep the 10k-pack index inside its size budget.
    """
    manifest = version.manifest()
    entry: dict[str, Any] = {
        "id": pack.id,
        "type": pack.type,
        "name": pack.name,
        "version": version.version,
        "schema": str(manifest.get("schema") or ASSET_SCHEMA),
        "license": pack.license,
        "published_at": version.published_at,
        "yanked": bool(version.yanked),
        "tags": [tag.tag for tag in pack.tags],
        "assets": [
            {
                "id": asset.id or f"asset-{asset.sha256[:16]}",
                "path": asset.path,
                "sha256": asset.sha256,
                "size": int(asset.size_bytes or 0),
                "size_bytes": int(asset.size_bytes or 0),
                "mime": asset.mime_type,
                "mime_type": asset.mime_type,
                "executable": bool(asset.executable),
                "sandbox": asset.sandbox,
            }
            for asset in version.files
        ],
    }
    optional = {
        "author": pack.author,
        "description": pack.description,
        "source_url": pack.source_url,
        "confidence": pack.confidence,
        "capabilities": pack.capabilities(),
        "permissions": pack.permissions(),
        "min_ozma_version": version.min_ozma_version,
        "min_display_schema": version.min_display_schema,
        "signature": version.signature,
        "preview": str(manifest.get("preview") or ""),
    }
    for key, value in optional.items():
        if value not in (None, "", [], {}):
            entry[key] = value
    for key in ("provenance", "vibe", "shape", "coherence"):
        value = manifest.get(key)
        if value not in (None, "", [], {}):
            entry[key] = value
    return entry


def build_index(
    engine,
    *,
    channel: str,
    generated_at: str | datetime,
    signer: Signer | None = None,
    include_private: bool = False,
    review_required: bool = False,
) -> dict[str, Any]:
    """Build a deterministic, optionally signed index for ``channel``.

    When ``review_required`` is true, only approved versions are included.
    Versions without a review row count as approved for backward compatibility.
    """
    requested = str(channel or "stable").strip().lower()
    with Session(engine) as session:
        statement = select(AssetPack).options(
            selectinload(AssetPack.versions).selectinload(AssetVersion.files),
            selectinload(AssetPack.tags),
            selectinload(AssetPack.provenance),
        )
        packs = session.execute(statement).scalars().all()
        statuses: dict[tuple[str, str], str] = {}
        if review_required:
            statuses = {
                (row.pack_id, row.version): row.status
                for row in session.execute(select(AssetReview)).scalars()
            }
        entries: list[dict[str, Any]] = []
        for pack in packs:
            if pack.private and not include_private:
                continue
            for version in sorted(
                pack.versions,
                key=lambda item: version_key(item.version),
            ):
                if review_required:
                    status = statuses.get(
                        (pack.id, version.version),
                        "approved",
                    )
                    if status != "approved":
                        continue
                manifest = version.manifest()
                declared = _declared_channel(manifest, pack)
                if requested != "all":
                    if version.yanked:
                        continue
                    if requested not in {declared, "all"}:
                        continue
                entries.append(_index_entry(pack, version))

    entries.sort(key=lambda item: (str(item.get("id", "")), version_key(
        str(item.get("version", ""))
    )))
    content_hash = sha256_hex(canonical_json(entries))
    index: dict[str, Any] = {
        "schema": INDEX_SCHEMA,
        "channel": requested,
        "generated_at": _generated_at(generated_at),
        "content_hash": content_hash,
        "etag": f'"{content_hash}"',
        "entries": entries,
        _SIGNATURE_KEY: None,
    }
    if signer is not None:
        unsigned = {key: value for key, value in index.items()
                    if key != _SIGNATURE_KEY}
        signature = signer.sign(canonical_json(unsigned))
        index[_SIGNATURE_KEY] = {
            "algorithm": getattr(signer, "algorithm", ""),
            "key_id": getattr(signer, "key_id", ""),
            "value": signature,
        }
    return index


def index_bytes(index: Mapping[str, Any]) -> bytes:
    """Return canonical bytes for an index payload (the wire body)."""
    return canonical_json(index)


def verify_index(
    index: Mapping[str, Any],
    verifier: Verifier | None,
) -> bool:
    """Verify the content hash, ETag, and detached signature of an index."""
    if not isinstance(index, Mapping):
        return False
    entries = index.get("entries")
    if not isinstance(entries, list):
        return False
    expected_hash = sha256_hex(canonical_json(entries))
    content_hash = str(index.get("content_hash") or "")
    if not content_hash or not hmac.compare_digest(content_hash, expected_hash):
        return False
    expected_etag = f'"{expected_hash}"'
    etag = index.get("etag")
    if etag is not None and str(etag) != expected_etag:
        return False

    signature = index.get(_SIGNATURE_KEY)
    if verifier is None:
        return False
    if isinstance(signature, Mapping):
        value = signature.get("value")
        algorithm = str(signature.get("algorithm") or "")
    elif isinstance(signature, str):
        value = signature
        algorithm = ""
    else:
        return False
    if not isinstance(value, str) or not value:
        return False
    verifier_algorithm = str(getattr(verifier, "algorithm", "") or "")
    if algorithm and verifier_algorithm and algorithm != verifier_algorithm:
        return False

    unsigned = {key: value for key, value in index.items()
                if key != _SIGNATURE_KEY}
    payload = canonical_json(unsigned)
    try:
        if hasattr(verifier, "verify"):
            result = verifier.verify(payload, value)
        elif callable(verifier):
            result = verifier(payload, value)
        else:
            return False
    except Exception:
        return False
    return bool(result)


# ── Service ──────────────────────────────────────────────────────────────────


@dataclass(frozen=True)
class SnapshotResult:
    """Snapshot result that supports both attribute and mapping access."""

    index: dict[str, Any] | None
    etag: str
    content_hash: str
    generated_at: str
    not_modified: bool
    status_code: int

    def to_dict(self) -> dict[str, Any]:
        return {
            "index": self.index,
            "etag": self.etag,
            "content_hash": self.content_hash,
            "generated_at": self.generated_at,
            "not_modified": self.not_modified,
            "status_code": self.status_code,
        }

    def __getitem__(self, key: str) -> Any:
        return self.to_dict()[key]

    def __iter__(self):
        return iter(self.to_dict())

    def __len__(self) -> int:
        return len(self.to_dict())

    def get(self, key: str, default: Any = None) -> Any:
        return self.to_dict().get(key, default)

    def keys(self):
        return self.to_dict().keys()

    def items(self):
        return self.to_dict().items()

    @property
    def headers(self) -> dict[str, str]:
        return {
            "ETag": self.etag,
            "X-Content-SHA256": self.content_hash,
            "X-Generated-At": self.generated_at,
        }


def _normalise_etag(value: str) -> str:
    text = str(value or "").strip()
    if text.startswith("W/"):
        text = text[2:].strip()
    if len(text) >= 2 and text[0] == '"' and text[-1] == '"':
        text = text[1:-1]
    return text


def _etag_matches(header: str | None, etag: str) -> bool:
    if not header:
        return False
    wanted = _normalise_etag(etag)
    for candidate in str(header).split(","):
        text = candidate.strip()
        if text == "*":
            return True
        if _normalise_etag(text) == wanted:
            return True
    return False


def _attestation_given(attestation: Any) -> bool:
    if attestation is None or attestation is False:
        return False
    if isinstance(attestation, str):
        return bool(attestation.strip()) and attestation.strip().lower() not in {
            "0",
            "false",
            "no",
            "off",
        }
    if isinstance(attestation, Mapping):
        return bool(attestation)
    return bool(attestation)


class AssetService:
    """Submission, search, stats, and snapshot operations for Connect."""

    def __init__(
        self,
        engine,
        *,
        store: ObjectStore | None = None,
        signer: Signer | None = None,
        verifier: Verifier | None = None,
        allowed_licenses: frozenset[str] | set[str] | None = None,
        review_required: bool = False,
        review_token: str | None = None,
    ) -> None:
        self.engine = engine
        self.store = store
        self.signer = signer
        self.verifier = verifier
        self.allowed_licenses = frozenset(
            allowed_licenses or DEFAULT_ALLOWED_LICENSES
        )
        self.review_required = bool(review_required)
        self.review_token = str(review_token).strip() if review_token else None
        self.reviews = ReviewService(engine, required=self.review_required)

    # Submission ---------------------------------------------------------

    @staticmethod
    def _normalise_assets(raw_assets: Any) -> list[dict[str, Any]]:
        if raw_assets in (None, ""):
            return []
        if not isinstance(raw_assets, list):
            raise InvalidAssetError("manifest assets must be a list")
        assets: list[dict[str, Any]] = []
        for raw in raw_assets:
            if not isinstance(raw, Mapping):
                raise InvalidAssetError("asset entry must be an object")
            path = str(raw.get("path") or "").strip()
            if not path:
                raise InvalidAssetError("asset entry missing path")
            validate_key(path)
            try:
                size = int(raw.get("size", raw.get("size_bytes", 0)) or 0)
            except (TypeError, ValueError):
                raise InvalidAssetError(
                    f"invalid asset size for {path!r}"
                ) from None
            if size < 0:
                raise InvalidAssetError(f"invalid asset size for {path!r}")
            executable = as_bool(raw.get("executable", False))
            sandbox_value = raw.get("sandbox")
            sandbox = str(sandbox_value) if sandbox_value else None
            if executable and sandbox not in _EXECUTABLE_SANDBOXES:
                raise InvalidAssetError(
                    f"executable asset {path!r} requires an iframe or wasm sandbox"
                )
            known = {
                "path",
                "sha256",
                "size",
                "size_bytes",
                "mime",
                "mime_type",
                "executable",
                "sandbox",
            }
            extra = {key: value for key, value in raw.items()
                     if key not in known}
            assets.append({
                "path": path,
                "sha256": str(raw.get("sha256") or "").lower(),
                "size": size,
                "mime": str(
                    raw.get("mime", raw.get("mime_type", "")) or ""
                ),
                "executable": executable,
                "sandbox": sandbox,
                "_extra": extra,
            })
        return assets

    def submit(
        self,
        manifest: Mapping[str, Any],
        *,
        attestation: Any = None,
        private: bool | None = None,
    ) -> dict[str, Any]:
        if not isinstance(manifest, Mapping):
            raise InvalidAssetError("manifest must be a JSON object")
        pack_id = str(manifest.get("id") or "").strip()
        if not pack_id:
            raise InvalidAssetError("manifest missing id")
        if not _ID_RE.match(pack_id):
            raise InvalidAssetError(f"invalid pack id: {pack_id!r}")

        pack_type = str(manifest.get("type") or "widget_pack").strip()
        if pack_type not in PACK_TYPES:
            raise InvalidAssetError(f"unknown asset type: {pack_type!r}")
        version = str(manifest.get("version") or "1.0.0").strip()
        if not version:
            raise InvalidAssetError("manifest missing version")

        provenance = as_mapping(manifest.get("provenance"))
        if not provenance:
            raise InvalidAssetError("submission requires provenance")
        first_party = as_bool(provenance.get("first_party")) or as_bool(
            manifest.get("first_party")
        )

        tags = as_str_list(manifest.get("tags"))
        capabilities = as_str_list(manifest.get("capabilities"))
        permissions = as_str_list(manifest.get("permissions"))
        licence = normalize_license(
            str(manifest.get("license", manifest.get("licence", "")) or "")
        )

        if private is not None:
            is_private = as_bool(private, True)
        elif manifest.get("private") is not None:
            is_private = as_bool(manifest.get("private"), True)
        elif manifest.get("public") is not None:
            is_private = not as_bool(manifest.get("public"), False)
        else:
            is_private = True
        public = not is_private

        if public:
            if not license_is_allowed(
                licence,
                self.allowed_licenses,
                first_party=first_party,
            ):
                raise InvalidAssetError(
                    f"licence {licence or '<missing>'!r} is not on the "
                    "publish allowlist"
                )
            if not _attestation_given(attestation):
                raise InvalidAssetError(
                    "public submission requires explicit attestation"
                )

        assets = self._normalise_assets(manifest.get("assets", []))

        if is_private or first_party:
            review_status = "approved"
        elif self.review_required:
            review_status = "pending"
        else:
            review_status = "approved"

        now = utc_now_iso()
        published_at = str(manifest.get("published_at") or now)
        yanked = as_bool(manifest.get("yanked", False))
        stored = dict(manifest)
        stored.update({
            "id": pack_id,
            "type": pack_type,
            "name": str(manifest.get("name") or pack_id),
            "version": version,
            "schema": str(manifest.get("schema") or ASSET_SCHEMA),
            "author": str(manifest.get("author") or ""),
            "description": str(manifest.get("description") or ""),
            "license": licence,
            "source_url": str(manifest.get("source_url") or ""),
            "tags": tags,
            "capabilities": capabilities,
            "permissions": permissions,
            "min_ozma_version": str(manifest.get("min_ozma_version") or ""),
            "min_display_schema": str(
                manifest.get("min_display_schema") or ""
            ),
            "assets": [
                {key: value for key, value in asset.items()
                 if key != "_extra"} | asset["_extra"]
                for asset in assets
            ],
            "provenance": provenance,
            "confidence": str(manifest.get("confidence") or ""),
            "signature": str(manifest.get("signature") or ""),
            "published_at": published_at,
            "yanked": yanked,
            "private": is_private,
        })
        if "licence" in manifest:
            stored["licence"] = licence

        manifest_bytes = json_dumps(stored).encode("utf-8")
        manifest_hash = sha256_hex(manifest_bytes)
        manifest_key = None
        if self.store is not None:
            manifest_key = put_content(
                self.store,
                manifest_bytes,
                prefix="manifests",
                suffix=".json",
                content_type="application/json",
            )

        checksums = {
            asset["path"]: asset["sha256"]
            for asset in assets
            if asset["sha256"]
        }
        summary = manifest_summary(pack_id, stored)

        try:
            with Session(self.engine) as session:
                pack = session.get(AssetPack, pack_id)
                if pack is None:
                    pack = AssetPack(id=pack_id, created_at=now)
                    session.add(pack)
                else:
                    existing = session.get(
                        AssetVersion,
                        {"pack_id": pack_id, "version": version},
                    )
                    if existing is not None:
                        raise AssetConflictError(
                            f"pack {pack_id!r} version {version!r} "
                            "already exists"
                        )

                latest_key = max(
                    (version_key(item.version) for item in pack.versions),
                    default=None,
                )
                summary_is_current = (
                    latest_key is None or version_key(version) >= latest_key
                )
                summary_visible = summary_is_current and (
                    not self.review_required
                    or review_status == "approved"
                    or latest_key is None
                )
                if summary_visible:
                    apply_pack_summary(pack, summary, now=now)
                else:
                    # A pending version must still be publicly recognised as a
                    # submission (otherwise reviewers cannot find it), but it
                    # must not replace the metadata of an older approved version.
                    pack.private = is_private

                version_row = AssetVersion(
                    pack_id=pack_id,
                    version=version,
                    manifest_json=json_dumps(stored),
                    checksums_json=json_dumps(checksums),
                    signature=stored["signature"],
                    min_ozma_version=stored["min_ozma_version"],
                    min_display_schema=stored["min_display_schema"],
                    published_at=published_at,
                    yanked=yanked,
                    created_at=now,
                )
                version_row.files = [
                    AssetFile(
                        pack_id=pack_id,
                        version=version,
                        path=asset["path"],
                        sha256=asset["sha256"],
                        size_bytes=asset["size"],
                        mime_type=asset["mime"],
                        executable=asset["executable"],
                        sandbox=asset["sandbox"],
                        extra_json=json_dumps(asset["_extra"]),
                    )
                    for asset in assets
                ]
                session.add(version_row)
                session.add(AssetReview(
                    pack_id=pack_id,
                    version=version,
                    status=review_status,
                    reviewer="system" if review_status == "approved" else "",
                    note=(
                        "automatic approval"
                        if review_status == "approved"
                        else ""
                    ),
                    reviewed_at=now if review_status == "approved" else "",
                    created_at=now,
                    updated_at=now,
                ))

                session.commit()
        except AssetError:
            raise
        except Exception as exc:  # Integrity and driver errors are conflicts.
            from sqlalchemy.exc import IntegrityError

            if isinstance(exc, IntegrityError):
                raise AssetConflictError(
                    f"pack {pack_id!r} version {version!r} already exists"
                ) from exc
            raise

        return {
            "ok": True,
            "id": pack_id,
            "version": version,
            "status": "private" if is_private else "pending_review",
            "private": is_private,
            "review_status": review_status,
            "review_required": self.review_required,
            "license": licence,
            "manifest_sha256": manifest_hash,
            "manifest_key": manifest_key,
            "submitted_at": now,
        }

    def submit_request(self, payload: Mapping[str, Any]) -> dict[str, Any]:
        """Accept either ``{manifest, attestation}`` or a bare manifest."""
        if not isinstance(payload, Mapping):
            raise InvalidAssetError("submission body must be a JSON object")
        source = payload.get("manifest")
        if isinstance(source, Mapping):
            manifest = dict(source)
            attestation = payload.get("attestation")
            private = payload.get("private")
            if private is None and payload.get("public") is not None:
                private = not as_bool(payload.get("public"), False)
        else:
            manifest = dict(payload)
            attestation = manifest.pop("attestation", None)
            private = manifest.get("private")
        return self.submit(
            manifest,
            attestation=attestation,
            private=private,
        )

    # Queries ------------------------------------------------------------

    def _review_statuses(
        self,
        session: Session,
    ) -> dict[tuple[str, str], str]:
        if not self.review_required:
            return {}
        rows = session.execute(select(AssetReview)).scalars().all()
        return {(row.pack_id, row.version): row.status for row in rows}

    def _latest_version(
        self,
        pack: AssetPack,
        *,
        include_yanked: bool,
        review_statuses: Mapping[tuple[str, str], str] | None = None,
    ) -> AssetVersion | None:
        versions = sorted(
            pack.versions,
            key=lambda item: version_key(item.version),
            reverse=True,
        )
        if not include_yanked:
            versions = [item for item in versions if not item.yanked]
        if self.review_required:
            statuses = review_statuses or {}
            versions = [
                item
                for item in versions
                if statuses.get((pack.id, item.version), "approved")
                == "approved"
            ]
        return versions[0] if versions else None

    @staticmethod
    def _pagination(limit: int, offset: int) -> tuple[int, int]:
        try:
            limit = int(limit)
            offset = int(offset)
        except (TypeError, ValueError):
            raise InvalidAssetError("limit and offset must be integers") from None
        if limit < 0 or offset < 0:
            raise InvalidAssetError("limit and offset cannot be negative")
        return min(limit, _MAX_PAGE_SIZE), offset

    def search(
        self,
        q: str | None = None,
        *,
        type: str | None = None,
        tag: str | None = None,
        licence: str | None = None,
        permission: str | None = None,
        include_yanked: bool = False,
        limit: int = 50,
        offset: int = 0,
        include_private: bool = False,
    ) -> dict[str, Any]:
        if type is not None and type not in PACK_TYPES:
            raise InvalidAssetError(f"unknown asset type: {type!r}")
        limit, offset = self._pagination(limit, offset)
        tokens = [token for token in (q or "").lower().split() if token]
        wanted_tag = (tag or "").strip().lower() or None
        wanted_permission = (permission or "").strip().lower() or None
        wanted_licence = (
            normalize_license(licence).lower() if licence else None
        )

        results: list[dict[str, Any]] = []
        with Session(self.engine) as session:
            statement = select(AssetPack).options(
                selectinload(AssetPack.versions).selectinload(
                    AssetVersion.files
                ),
                selectinload(AssetPack.tags),
                selectinload(AssetPack.provenance),
            )
            packs = session.execute(statement).scalars().all()
            review_statuses = self._review_statuses(session)
            for pack in packs:
                if pack.private and not include_private:
                    continue
                if type is not None and pack.type != type:
                    continue
                if wanted_licence is not None:
                    if normalize_license(pack.license).lower() != wanted_licence:
                        continue
                lowered_tags = {item.tag.lower() for item in pack.tags}
                if wanted_tag is not None and wanted_tag not in lowered_tags:
                    continue
                lowered_permissions = {
                    item.lower() for item in pack.permissions()
                }
                if (
                    wanted_permission is not None
                    and wanted_permission not in lowered_permissions
                ):
                    continue
                version = self._latest_version(
                    pack,
                    include_yanked=include_yanked,
                    review_statuses=review_statuses,
                )
                if version is None:
                    continue
                entry = _entry_payload(pack, version)
                if tokens:
                    blob = " ".join([
                        pack.search_text,
                        canonical_json(entry).decode("utf-8").lower(),
                    ]).lower()
                    if not all(token in blob for token in tokens):
                        continue
                results.append(entry)

        results.sort(key=lambda item: (
            str(item.get("name", "")).lower(),
            str(item.get("id", "")),
        ))
        total = len(results)
        page = results[offset:offset + limit] if limit else []
        return {
            "entries": page,
            "count": len(page),
            "total": total,
            "query": q,
            "limit": limit,
            "offset": offset,
        }

    def entry(
        self,
        pack_id: str,
        version: str | None = None,
        *,
        include_private: bool = False,
    ) -> dict[str, Any]:
        with Session(self.engine) as session:
            statement = select(AssetPack).where(AssetPack.id == pack_id).options(
                selectinload(AssetPack.versions).selectinload(
                    AssetVersion.files
                ),
                selectinload(AssetPack.tags),
                selectinload(AssetPack.provenance),
            )
            pack = session.execute(statement).scalar_one_or_none()
            if pack is None or (pack.private and not include_private):
                raise AssetNotFoundError(f"unknown pack {pack_id!r}")
            review_statuses = self._review_statuses(session)
            if version is not None:
                found = next(
                    (item for item in pack.versions
                     if item.version == version),
                    None,
                )
                if found is None:
                    raise AssetNotFoundError(
                        f"unknown pack {pack_id!r} version {version!r}"
                    )
                if self.review_required and review_statuses.get(
                    (pack.id, found.version),
                    "approved",
                ) != "approved":
                    raise AssetNotFoundError(
                        f"unknown pack {pack_id!r} version {version!r}"
                    )
                return _entry_payload(pack, found)
            latest = self._latest_version(
                pack,
                include_yanked=False,
                review_statuses=review_statuses,
            )
            if latest is None:
                latest = self._latest_version(
                    pack,
                    include_yanked=True,
                    review_statuses=review_statuses,
                )
            if latest is None:
                raise AssetNotFoundError(f"unknown pack {pack_id!r}")
            return _entry_payload(pack, latest)

    def versions(
        self,
        pack_id: str,
        *,
        include_yanked: bool = True,
    ) -> dict[str, Any]:
        with Session(self.engine) as session:
            statement = select(AssetPack).where(AssetPack.id == pack_id).options(
                selectinload(AssetPack.versions).selectinload(
                    AssetVersion.files
                ),
                selectinload(AssetPack.tags),
                selectinload(AssetPack.provenance),
            )
            pack = session.execute(statement).scalar_one_or_none()
            if pack is None or pack.private:
                raise AssetNotFoundError(f"unknown pack {pack_id!r}")
            review_statuses = self._review_statuses(session)
            versions = sorted(
                pack.versions,
                key=lambda item: version_key(item.version),
                reverse=True,
            )
            if self.review_required:
                versions = [
                    item
                    for item in versions
                    if review_statuses.get((pack.id, item.version), "approved")
                    == "approved"
                ]
            if not include_yanked:
                versions = [item for item in versions if not item.yanked]
            if not versions:
                raise AssetNotFoundError(f"unknown pack {pack_id!r}")
            return {
                "id": pack_id,
                "versions": [
                    _entry_payload(pack, version) for version in versions
                ],
                "count": len(versions),
            }

    def categories(self) -> dict[str, Any]:
        type_counts: dict[str, int] = {}
        tag_counts: dict[str, int] = {}
        permission_counts: dict[str, int] = {}
        licence_counts: dict[str, int] = {}
        with Session(self.engine) as session:
            statement = select(AssetPack).options(
                selectinload(AssetPack.versions),
                selectinload(AssetPack.tags),
            )
            packs = session.execute(statement).scalars().all()
            review_statuses = self._review_statuses(session)
            for pack in packs:
                if pack.private:
                    continue
                if self._latest_version(
                    pack,
                    include_yanked=False,
                    review_statuses=review_statuses,
                ) is None:
                    continue
                type_counts[pack.type] = type_counts.get(pack.type, 0) + 1
                licence_key = normalize_license(pack.license) or "unknown"
                licence_counts[licence_key] = (
                    licence_counts.get(licence_key, 0) + 1
                )
                for tag in {item.tag for item in pack.tags}:
                    tag_counts[tag] = tag_counts.get(tag, 0) + 1
                for permission in set(pack.permissions()):
                    permission_counts[permission] = (
                        permission_counts.get(permission, 0) + 1
                    )
        categories = [
            {"id": key, "kind": "type", "count": count}
            for key, count in sorted(type_counts.items())
        ]
        categories.extend(
            {"id": key, "kind": "tag", "count": count}
            for key, count in sorted(tag_counts.items())
        )
        licences = dict(sorted(licence_counts.items()))
        return {
            "types": dict(sorted(type_counts.items())),
            "tags": dict(sorted(tag_counts.items())),
            "permissions": dict(sorted(permission_counts.items())),
            "licences": licences,
            "licenses": dict(licences),
            "categories": categories,
        }

    def stats(self) -> dict[str, Any]:
        type_counts: dict[str, int] = {}
        tag_counts: dict[str, int] = {}
        permission_counts: dict[str, int] = {}
        licence_counts: dict[str, int] = {}
        pack_ids: set[str] = set()
        yanked_versions = 0
        total_versions = 0
        total_bytes = 0
        private_packs = 0
        with Session(self.engine) as session:
            statement = select(AssetPack).options(
                selectinload(AssetPack.versions).selectinload(
                    AssetVersion.files
                ),
                selectinload(AssetPack.tags),
            )
            packs = session.execute(statement).scalars().all()
            review_statuses = self._review_statuses(session)
            for pack in packs:
                if pack.private:
                    private_packs += 1
                    continue
                versions = list(pack.versions)
                if self.review_required:
                    versions = [
                        item
                        for item in versions
                        if review_statuses.get(
                            (pack.id, item.version),
                            "approved",
                        ) == "approved"
                    ]
                    if not versions:
                        continue
                pack_ids.add(pack.id)
                licence_key = normalize_license(pack.license) or "unknown"
                licence_counts[licence_key] = (
                    licence_counts.get(licence_key, 0) + 1
                )
                for tag in {item.tag for item in pack.tags}:
                    tag_counts[tag] = tag_counts.get(tag, 0) + 1
                for permission in set(pack.permissions()):
                    permission_counts[permission] = (
                        permission_counts.get(permission, 0) + 1
                    )
                for version in versions:
                    total_versions += 1
                    type_counts[pack.type] = type_counts.get(pack.type, 0) + 1
                    if version.yanked:
                        yanked_versions += 1
                    total_bytes += sum(
                        max(0, int(asset.size_bytes or 0))
                        for asset in version.files
                    )
        by_license = dict(sorted(licence_counts.items()))
        return {
            "packs": len(pack_ids),
            "versions": total_versions,
            "total_packs": len(pack_ids),
            "total_versions": total_versions,
            "by_type": dict(sorted(type_counts.items())),
            "by_tag": dict(sorted(tag_counts.items())),
            "by_permission": dict(sorted(permission_counts.items())),
            "by_license": by_license,
            "by_licence": dict(by_license),
            "yanked_versions": yanked_versions,
            "private_packs": private_packs,
            "total_bytes": total_bytes,
            "total_size_bytes": total_bytes,
        }

    # Sync support -------------------------------------------------------

    def snapshot(
        self,
        channel: str,
        if_none_match: str | None = None,
    ) -> SnapshotResult:
        index = build_index(
            self.engine,
            channel=channel,
            generated_at=utc_now_iso(),
            signer=self.signer,
            include_private=False,
            review_required=self.review_required,
        )
        etag = str(index["etag"])
        not_modified = _etag_matches(if_none_match, etag)
        return SnapshotResult(
            index=None if not_modified else index,
            etag=etag,
            content_hash=str(index["content_hash"]),
            generated_at=str(index["generated_at"]),
            not_modified=not_modified,
            status_code=304 if not_modified else 200,
        )


__all__ = [
    "HAVE_ED25519",
    "Ed25519Signer",
    "Ed25519Verifier",
    "HMACSigner",
    "HMACVerifier",
    "Signer",
    "SnapshotResult",
    "Verifier",
    "AssetService",
    "build_index",
    "canonical_json",
    "index_bytes",
    "verify_index",
]
