# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Local display asset repository: index, cache, install, update, and sync.

This module is deliberately self-contained.  It extends the existing
``controller/widget_packs/`` layout without replacing ``WidgetPackManager`` and
never imports or executes pack code.

Storage layout
--------------

``controller/widget_packs/{id}/`` is the live installed copy.  The repository
keeps its own cache beside it:

``controller/asset_cache/index.json``
    Searchable, versioned index metadata with ``last_sync``/ETag/hash.
``controller/asset_cache/packs/{id}/{version}/``
    Immutable snapshots used for offline installs and rollback.
``controller/asset_cache/installed.json``
    Current/previous installed versions.
``controller/asset_cache/submissions.json``
    Local record of packs queued for community submission.

Security rules
--------------

* Archives are read member-by-member; ``extractall`` is never used.
* Absolute paths, ``..``, symlinks, hardlinks, device/FIFO entries, excessive
  depth/count/size, and undeclared payload files fail closed.
* Every declared asset hash is verified before a pack is installed.
* Signatures are verified with an injectable callable.  A manifest carrying a
  signature but no configured verifier fails closed; unsigned packs are only
  accepted when policy allows it.
* Yanked versions can be installed explicitly, but never automatically.
* Pack code is written to disk and never imported or executed here.
"""

from __future__ import annotations

import hashlib
import inspect
import io
import json
import logging
import os
import re
import shutil
import stat
import tarfile
import tempfile
import threading
import uuid
import zipfile
from collections import Counter
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path, PurePosixPath
from typing import Any
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen

from fastapi import APIRouter, Body, HTTPException, Query

log = logging.getLogger("ozma.asset_repository")

ASSET_SCHEMA = "org.ozma.asset-pack/1"
INDEX_SCHEMA = "org.ozma.asset-index/1"

PACK_TYPES = frozenset({
    "widget_pack",
    "theme_pack",
    "description_pack",
    "import_pack",
})

SYNC_MODES = frozenset({"full", "installed", "manual", "offline"})

#: Licences accepted for public submission.  Local/manual installs are not
#: gated by this list; only ``submit`` enforces it.
DEFAULT_ALLOWED_LICENSES = frozenset({
    "MIT",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "ISC",
    "Apache-2.0",
    "OFL-1.1",
    "CC0-1.0",
})

_LICENSE_ALIASES = {
    "apache 2.0": "Apache-2.0",
    "apache-2": "Apache-2.0",
    "bsd": "BSD-3-Clause",
    "cc0": "CC0-1.0",
    "mit license": "MIT",
    "ofl": "OFL-1.1",
}

_METADATA_FILENAMES = frozenset({
    "manifest.json",
    "options.json",
    "checksums.json",
    "LICENSE",
    "LICENSE.txt",
    "NOTICE",
    "README",
    "README.md",
    "preview.png",
})

_ID_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")


# ── Errors ───────────────────────────────────────────────────────────────────


class AssetRepositoryError(Exception):
    """Base class for errors that map to a typed HTTP response."""

    status_code = 400
    code = "asset_error"


class InvalidAssetError(AssetRepositoryError):
    """Malformed manifest, bad query, unsafe archive, or failed verification."""

    status_code = 400
    code = "invalid_asset"


class AssetNotFoundError(AssetRepositoryError):
    """Unknown pack, version, or cached payload."""

    status_code = 404
    code = "asset_not_found"


class AssetConflictError(AssetRepositoryError):
    """Yanked pack, unsigned pack, missing payload, or incompatible state."""

    status_code = 409
    code = "asset_conflict"


class AssetSignatureError(InvalidAssetError):
    """A required signature is missing or invalid."""

    code = "invalid_signature"


class AssetHashError(InvalidAssetError):
    """A file did not match its declared SHA-256."""

    code = "hash_mismatch"


class UnsafeArchiveError(InvalidAssetError):
    """An archive member violated path/type/size rules."""

    code = "unsafe_archive"


class AssetUpstreamError(AssetRepositoryError):
    """The configured sync/download transport failed."""

    status_code = 502
    code = "asset_upstream_error"


# ── Helpers ──────────────────────────────────────────────────────────────────


def _utc_now() -> str:
    return datetime.now(timezone.utc).isoformat()


def _canonical_json(value: Any) -> bytes:
    return json.dumps(
        value,
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")


def _json_bytes(value: Any) -> bytes:
    return (json.dumps(value, ensure_ascii=False, indent=2, sort_keys=True) + "\n").encode(
        "utf-8"
    )


def _as_mapping(value: Any) -> dict[str, Any]:
    return dict(value) if isinstance(value, Mapping) else {}


def _as_str_list(value: Any) -> list[str]:
    if isinstance(value, str):
        return [value] if value else []
    if isinstance(value, Sequence) and not isinstance(value, (bytes, bytearray)):
        return [str(item) for item in value if isinstance(item, (str, int, float))]
    return []


def _as_bool(value: Any, default: bool = False) -> bool:
    if value is None:
        return default
    if isinstance(value, bool):
        return value
    if isinstance(value, str):
        return value.strip().lower() not in {"", "0", "false", "no", "off"}
    return bool(value)


def _invoke_verifier(
    verifier: Callable[..., bool],
    manifest_bytes: bytes,
    signature: str,
    files: Mapping[str, bytes],
) -> bool:
    """Call a verifier with 3, 2, or 1 positional arguments.

    The canonical signature is ``(manifest_bytes, signature, files)``.  The
    narrower forms are accepted so a simple detached-signature verifier can be
    supplied without an adapter.
    """
    try:
        parameters = inspect.signature(verifier).parameters.values()
    except (TypeError, ValueError):
        return verifier(manifest_bytes, signature, files)
    positional = [
        parameter
        for parameter in parameters
        if parameter.kind
        in (parameter.POSITIONAL_ONLY, parameter.POSITIONAL_OR_KEYWORD)
    ]
    if any(parameter.kind == parameter.VAR_POSITIONAL for parameter in parameters):
        return verifier(manifest_bytes, signature, files)
    if len(positional) >= 3:
        return verifier(manifest_bytes, signature, files)
    if len(positional) == 2:
        return verifier(manifest_bytes, signature)
    if len(positional) == 1:
        return verifier(manifest_bytes)
    return verifier()


def _version_key(version: str) -> tuple[int, int, int, int, str]:
    """A forgiving semantic-version sort key (never raises)."""
    text = str(version or "0").strip()
    main, _, prerelease = text.partition("-")
    parts: list[int] = []
    for chunk in main.split("."):
        match = re.match(r"^(\d+)", chunk)
        parts.append(int(match.group(1)) if match else 0)
    while len(parts) < 3:
        parts.append(0)
    return (parts[0], parts[1], parts[2], 1 if not prerelease else 0, prerelease)


def _sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _etag_for(data: bytes) -> str:
    return '"' + _sha256(data) + '"'


def _flatten_text(value: Any) -> set[str]:
    """Collect lower-case words from arbitrary manifest metadata values."""
    found: set[str] = set()
    if isinstance(value, str):
        found.add(value.strip().lower())
    elif isinstance(value, Mapping):
        for item in value.values():
            found.update(_flatten_text(item))
    elif isinstance(value, Sequence) and not isinstance(value, (bytes, bytearray, str)):
        for item in value:
            found.update(_flatten_text(item))
    return {item for item in found if item}


def _normalize_license(expression: str) -> str:
    text = (expression or "").strip()
    return _LICENSE_ALIASES.get(text.lower(), text)


def license_is_allowed(
    expression: str,
    allowed: frozenset[str] | set[str] | None = None,
) -> bool:
    """Return whether an SPDX-ish expression is on the publish allowlist.

    Simple ``AND``/``OR`` expressions are handled conservatively: ``AND`` needs
    every component allowed, ``OR`` needs at least one.  Parentheses are
    stripped.  Unknown or missing licences are rejected.
    """
    allowlist = frozenset(allowed or DEFAULT_ALLOWED_LICENSES)
    text = _normalize_license(expression)
    if not text:
        return False
    if text in allowlist:
        return True

    text = text.replace("(", " ").replace(")", " ").strip()
    if " OR " in text:
        return any(license_is_allowed(part, allowlist) for part in text.split(" OR "))
    if " AND " in text:
        return all(license_is_allowed(part, allowlist) for part in text.split(" AND "))
    return False


def _safe_relative_path(name: str, *, max_depth: int = 16) -> str:
    """Normalize and validate a pack-relative path."""
    if not isinstance(name, str) or not name:
        raise UnsafeArchiveError("empty path in pack")
    candidate = name.replace("\\", "/").strip()
    if not candidate:
        raise UnsafeArchiveError("empty path in pack")
    if candidate.startswith("/") or candidate.startswith("~"):
        raise UnsafeArchiveError(f"absolute path in pack: {name!r}")
    if re.match(r"^[A-Za-z]:", candidate):
        raise UnsafeArchiveError(f"drive-qualified path in pack: {name!r}")
    path = PurePosixPath(candidate)
    parts = path.parts
    if any(part in ("", ".", "..") for part in parts):
        raise UnsafeArchiveError(f"path traversal in pack: {name!r}")
    if len(parts) > max_depth:
        raise UnsafeArchiveError(
            f"path depth exceeds {max_depth}: {name!r}"
        )
    return "/".join(parts)


def _extract_checksums(value: Any) -> dict[str, str]:
    """Read a permissive ``checksums``/``checksums.json`` structure."""
    if isinstance(value, Mapping):
        for key in ("files", "checksums", "assets"):
            nested = value.get(key)
            if isinstance(nested, (Mapping, list)):
                value = nested
                break
        else:
            value = {k: v for k, v in value.items() if k not in {"algorithm", "schema"}}
    if isinstance(value, list):
        result: dict[str, str] = {}
        for item in value:
            if not isinstance(item, Mapping):
                continue
            path = str(item.get("path") or "").strip()
            digest = str(item.get("sha256") or item.get("hash") or "").strip()
            if path and digest:
                result[path] = digest
        return result
    if not isinstance(value, Mapping):
        return {}
    result = {}
    for raw_path, raw_digest in value.items():
        path = str(raw_path).strip()
        if not path:
            continue
        if isinstance(raw_digest, Mapping):
            raw_digest = raw_digest.get("sha256") or raw_digest.get("hash") or ""
        digest = str(raw_digest or "").strip()
        if digest:
            result[path] = digest
    return result


def _read_limited(stream: Any, limit: int, name: str) -> bytes:
    chunks: list[bytes] = []
    total = 0
    while True:
        chunk = stream.read(min(64 * 1024, limit - total + 1))
        if not chunk:
            break
        total += len(chunk)
        if total > limit:
            raise UnsafeArchiveError(f"file exceeds size limit: {name!r}")
        chunks.append(chunk)
    return b"".join(chunks)


def _atomic_json_write(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp_name = tempfile.mkstemp(
        dir=str(path.parent), prefix=f".{path.name}.", suffix=".tmp"
    )
    tmp = Path(tmp_name)
    try:
        with os.fdopen(fd, "wb") as handle:
            handle.write(_json_bytes(value))
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(tmp, path)
    except Exception:
        tmp.unlink(missing_ok=True)
        raise


def _write_tree(destination: Path, files: Mapping[str, bytes], *, max_depth: int = 16) -> None:
    destination.mkdir(parents=True, exist_ok=True)
    for raw_name, data in files.items():
        name = _safe_relative_path(raw_name, max_depth=max_depth)
        target = destination.joinpath(*name.split("/"))
        target.parent.mkdir(parents=True, exist_ok=True)
        with open(target, "wb") as handle:
            handle.write(data)


def _atomic_tree_replace(
    destination: Path,
    files: Mapping[str, bytes],
    *,
    max_depth: int = 16,
) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    staging = Path(
        tempfile.mkdtemp(
            prefix=f".{destination.name}.tmp-", dir=str(destination.parent)
        )
    )
    try:
        _write_tree(staging, files, max_depth=max_depth)
        backup: Path | None = None
        if destination.exists():
            if destination.is_symlink():
                raise UnsafeArchiveError(f"refusing to replace symlink {destination}")
            backup = destination.parent / f".{destination.name}.old-{uuid.uuid4().hex}"
            os.replace(destination, backup)
        try:
            os.replace(staging, destination)
        except Exception:
            if backup is not None and not destination.exists():
                os.replace(backup, destination)
            raise
        if backup is not None:
            shutil.rmtree(backup, ignore_errors=True)
    finally:
        if staging.exists():
            shutil.rmtree(staging, ignore_errors=True)


# ── Manifest model ───────────────────────────────────────────────────────────


@dataclass
class AssetFile:
    """One hashed file in a pack manifest."""

    path: str
    sha256: str = ""
    size: int = 0
    mime: str = ""
    executable: bool = False
    sandbox: str | None = None
    extra: dict[str, Any] = field(default_factory=dict)

    _KNOWN = frozenset({
        "path",
        "sha256",
        "size",
        "size_bytes",
        "mime",
        "mime_type",
        "executable",
        "sandbox",
    })

    @classmethod
    def from_dict(cls, raw: Mapping[str, Any]) -> AssetFile:
        if not isinstance(raw, Mapping):
            raise InvalidAssetError("asset entry must be an object")
        path = str(raw.get("path") or "").strip()
        if not path:
            raise InvalidAssetError("asset entry missing path")
        size_value = raw.get("size", raw.get("size_bytes", 0))
        try:
            size = int(size_value or 0)
        except (TypeError, ValueError):
            raise InvalidAssetError(f"invalid asset size for {path!r}") from None
        return cls(
            path=path,
            sha256=str(raw.get("sha256") or ""),
            size=size,
            mime=str(raw.get("mime", raw.get("mime_type", "")) or ""),
            executable=_as_bool(raw.get("executable", False)),
            sandbox=(
                str(raw["sandbox"]) if raw.get("sandbox") not in (None, "") else None
            ),
            extra={k: v for k, v in raw.items() if k not in cls._KNOWN},
        )

    def to_dict(self) -> dict[str, Any]:
        data = dict(self.extra)
        data.update({
            "path": self.path,
            "sha256": self.sha256,
            "size": self.size,
            "mime": self.mime,
            "executable": self.executable,
            "sandbox": self.sandbox,
        })
        return data


@dataclass
class AssetPackEntry:
    """A manifest with lossless preservation of unknown fields."""

    id: str
    type: str = "widget_pack"
    name: str = ""
    version: str = "1.0.0"
    schema: str = ASSET_SCHEMA
    author: str = ""
    description: str = ""
    license: str = ""
    source_url: str = ""
    tags: list[str] = field(default_factory=list)
    capabilities: list[str] = field(default_factory=list)
    permissions: list[str] = field(default_factory=list)
    min_ozma_version: str = ""
    min_display_schema: str = ""
    assets: list[AssetFile] = field(default_factory=list)
    preview: str = ""
    provenance: dict[str, Any] = field(default_factory=dict)
    confidence: str = ""
    vibe: dict[str, Any] = field(default_factory=dict)
    shape: dict[str, Any] = field(default_factory=dict)
    coherence: dict[str, Any] = field(default_factory=dict)
    signature: str = ""
    published_at: str = ""
    yanked: bool = False
    extra: dict[str, Any] = field(default_factory=dict)

    _KNOWN = frozenset({
        "id",
        "type",
        "name",
        "version",
        "schema",
        "author",
        "description",
        "license",
        "licence",
        "source_url",
        "tags",
        "capabilities",
        "permissions",
        "min_ozma_version",
        "min_display_schema",
        "assets",
        "preview",
        "provenance",
        "confidence",
        "vibe",
        "shape",
        "coherence",
        "signature",
        "published_at",
        "yanked",
    })

    @classmethod
    def from_manifest(cls, manifest: Mapping[str, Any]) -> AssetPackEntry:
        if not isinstance(manifest, Mapping):
            raise InvalidAssetError("manifest must be a JSON object")
        pack_id = str(manifest.get("id") or "").strip()
        if not pack_id:
            raise InvalidAssetError("manifest missing id")
        if not _ID_RE.match(pack_id):
            raise InvalidAssetError(f"invalid pack id: {pack_id!r}")

        raw_assets = manifest.get("assets", [])
        if raw_assets in (None, ""):
            raw_assets = []
        if not isinstance(raw_assets, list):
            raise InvalidAssetError("manifest assets must be a list")
        assets = [AssetFile.from_dict(item) for item in raw_assets]

        pack_type = str(manifest.get("type") or "widget_pack").strip()
        return cls(
            id=pack_id,
            type=pack_type or "widget_pack",
            name=str(manifest.get("name") or pack_id),
            version=str(manifest.get("version") or "1.0.0"),
            schema=str(manifest.get("schema") or ASSET_SCHEMA),
            author=str(manifest.get("author") or ""),
            description=str(manifest.get("description") or ""),
            license=str(
                manifest.get("license", manifest.get("licence", "")) or ""
            ),
            source_url=str(manifest.get("source_url") or ""),
            tags=_as_str_list(manifest.get("tags")),
            capabilities=_as_str_list(manifest.get("capabilities")),
            permissions=_as_str_list(manifest.get("permissions")),
            min_ozma_version=str(manifest.get("min_ozma_version") or ""),
            min_display_schema=str(manifest.get("min_display_schema") or ""),
            assets=assets,
            preview=str(manifest.get("preview") or ""),
            provenance=_as_mapping(manifest.get("provenance")),
            confidence=str(manifest.get("confidence") or ""),
            vibe=_as_mapping(manifest.get("vibe")),
            shape=_as_mapping(manifest.get("shape")),
            coherence=_as_mapping(manifest.get("coherence")),
            signature=str(manifest.get("signature") or ""),
            published_at=str(manifest.get("published_at") or ""),
            yanked=_as_bool(manifest.get("yanked", False)),
            extra={k: v for k, v in manifest.items() if k not in cls._KNOWN},
        )

    def to_manifest(self) -> dict[str, Any]:
        data = dict(self.extra)
        data.update({
            "id": self.id,
            "type": self.type,
            "name": self.name,
            "version": self.version,
            "schema": self.schema,
            "author": self.author,
            "description": self.description,
            "license": self.license,
            "source_url": self.source_url,
            "tags": list(self.tags),
            "capabilities": list(self.capabilities),
            "permissions": list(self.permissions),
            "min_ozma_version": self.min_ozma_version,
            "min_display_schema": self.min_display_schema,
            "assets": [asset.to_dict() for asset in self.assets],
            "preview": self.preview,
            "provenance": dict(self.provenance),
            "confidence": self.confidence,
            "signature": self.signature,
            "published_at": self.published_at,
            "yanked": self.yanked,
        })
        # Optional design-language blocks are omitted when empty so that legacy
        # manifests do not gain claim-less objects on round-trip.
        for key, block in (
            ("vibe", self.vibe),
            ("shape", self.shape),
            ("coherence", self.coherence),
        ):
            if block:
                data[key] = dict(block)
        return data

    def to_dict(self) -> dict[str, Any]:
        return self.to_manifest()

    def keywords(self) -> set[str]:
        """Return tags plus all string values in vibe/shape/coherence."""
        found = {tag.strip().lower() for tag in self.tags if tag.strip()}
        for block in (self.vibe, self.shape, self.coherence):
            found.update(_flatten_text(block))
        return found


# ── Index ────────────────────────────────────────────────────────────────────


class AssetIndex:
    """In-memory index with atomic JSON persistence and offline metadata."""

    def __init__(
        self,
        entries: Sequence[AssetPackEntry | Mapping[str, Any]] | None = None,
        *,
        path: str | Path | None = None,
    ) -> None:
        self.path = Path(path) if path is not None else None
        self.last_sync: str | None = None
        self.etag: str = ""
        self.content_hash: str = ""
        self.signature: str = ""
        self.signature_meta: dict[str, Any] = {}
        self.extra: dict[str, Any] = {}
        self._entries: list[AssetPackEntry] = []
        for entry in entries or []:
            self.add(entry)

    # Persistence ---------------------------------------------------------

    def load(self, path: str | Path | None = None) -> AssetIndex:
        if not isinstance(self, AssetIndex):
            # ``AssetIndex.load(path)`` convenience form.
            return AssetIndex(path=self).load()
        if path is not None:
            self.path = Path(path)
        if self.path is None or not self.path.exists():
            return self
        try:
            payload = json.loads(self.path.read_text(encoding="utf-8"))
        except (OSError, ValueError) as exc:
            raise InvalidAssetError(f"cannot read index: {exc}") from exc
        self._apply_payload(payload)
        return self

    @classmethod
    def from_file(cls, path: str | Path) -> AssetIndex:
        return cls(path=path).load()

    def save(self, path: str | Path | None = None) -> None:
        if path is not None:
            self.path = Path(path)
        if self.path is None:
            raise InvalidAssetError("index path is not configured")
        self.content_hash = self.compute_hash()
        _atomic_json_write(self.path, self.to_dict())

    def to_dict(self) -> dict[str, Any]:
        data = dict(self.extra)
        data.update({
            "schema": INDEX_SCHEMA,
            "last_sync": self.last_sync,
            "etag": self.etag,
            "content_hash": self.content_hash,
            "signature": self.signature,
            "entries": [entry.to_manifest() for entry in self._entries],
        })
        return data

    def _apply_payload(self, payload: Any) -> None:
        if isinstance(payload, list):
            raw_entries: Any = payload
            metadata: Mapping[str, Any] = {}
        elif isinstance(payload, Mapping):
            metadata = payload
            raw_entries = payload.get("entries")
            if raw_entries is None:
                raw_entries = payload.get("packs")
            if raw_entries is None:
                raw_entries = payload.get("index")
            if raw_entries is None:
                raw_entries = []
        else:
            raise InvalidAssetError("index must be a JSON object or list")

        self.last_sync = metadata.get("last_sync") if isinstance(metadata, Mapping) else None
        self.etag = str(metadata.get("etag") or "") if isinstance(metadata, Mapping) else ""
        self.content_hash = (
            str(metadata.get("content_hash") or metadata.get("hash") or "")
            if isinstance(metadata, Mapping)
            else ""
        )
        self.signature = (
            str(metadata.get("signature") or "") if isinstance(metadata, Mapping) else ""
        )
        signature_meta = metadata.get("signature") if isinstance(metadata, Mapping) else None
        if isinstance(signature_meta, Mapping):
            self.signature = str(signature_meta.get("value") or "")
            self.signature_meta = dict(signature_meta)
        else:
            self.signature_meta = {}
        known = {"schema", "last_sync", "etag", "content_hash", "hash", "signature",
                 "entries", "packs", "index"}
        self.extra = (
            {k: v for k, v in metadata.items() if k not in known}
            if isinstance(metadata, Mapping)
            else {}
        )

        self._entries = []
        for item in self._entries_from(raw_entries):
            self.add(item, replace=False)

    @staticmethod
    def _entries_from(raw_entries: Any) -> list[Mapping[str, Any]]:
        if isinstance(raw_entries, Mapping):
            items: list[Mapping[str, Any]] = []
            for pack_id, value in raw_entries.items():
                if not isinstance(value, Mapping):
                    continue
                versions = value.get("versions")
                if isinstance(versions, Mapping):
                    for version, manifest in versions.items():
                        if not isinstance(manifest, Mapping):
                            continue
                        merged = dict(manifest)
                        merged.setdefault("id", pack_id)
                        merged.setdefault("version", version)
                        items.append(merged)
                else:
                    manifest = dict(value)
                    manifest.setdefault("id", pack_id)
                    items.append(manifest)
            return items
        if isinstance(raw_entries, list):
            return [item for item in raw_entries if isinstance(item, Mapping)]
        return []

    # Mutation ------------------------------------------------------------

    def add(
        self,
        entry: AssetPackEntry | Mapping[str, Any],
        *,
        replace: bool = True,
    ) -> AssetPackEntry:
        item = entry if isinstance(entry, AssetPackEntry) else AssetPackEntry.from_manifest(entry)
        for index, existing in enumerate(self._entries):
            if existing.id == item.id and existing.version == item.version:
                if replace:
                    self._entries[index] = item
                return item
        self._entries.append(item)
        return item

    def remove(self, pack_id: str, version: str | None = None) -> int:
        before = len(self._entries)
        if version is None:
            self._entries = [item for item in self._entries if item.id != pack_id]
        else:
            self._entries = [
                item
                for item in self._entries
                if not (item.id == pack_id and item.version == version)
            ]
        return before - len(self._entries)

    # Queries -------------------------------------------------------------

    @property
    def entries(self) -> list[AssetPackEntry]:
        return list(self._entries)

    def versions(
        self,
        pack_id: str,
        *,
        include_yanked: bool = True,
    ) -> list[AssetPackEntry]:
        items = [
            item
            for item in self._entries
            if item.id == pack_id and (include_yanked or not item.yanked)
        ]
        return sorted(items, key=lambda item: _version_key(item.version), reverse=True)

    def latest(
        self,
        pack_id: str,
        *,
        include_yanked: bool = False,
    ) -> AssetPackEntry | None:
        items = self.versions(pack_id)
        if not include_yanked:
            items = [item for item in items if not item.yanked]
        return items[0] if items else None

    def entry(self, pack_id: str, version: str | None = None) -> AssetPackEntry | None:
        if version is not None:
            for item in self._entries:
                if item.id == pack_id and item.version == version:
                    return item
            return None
        found = self.latest(pack_id)
        if found is None:
            # Keep explicit access to an all-yanked pack working.
            return self.latest(pack_id, include_yanked=True)
        return found

    def search(
        self,
        q: str | None = None,
        *,
        type: str | None = None,
        tag: str | None = None,
        permission: str | None = None,
        include_yanked: bool = False,
    ) -> list[AssetPackEntry]:
        if type is not None and type not in PACK_TYPES:
            raise InvalidAssetError(f"unknown asset type: {type!r}")

        tokens = [token for token in (q or "").lower().split() if token]
        wanted_tag = (tag or "").strip().lower() or None
        wanted_permission = (permission or "").strip().lower() or None

        latest_by_id: dict[str, AssetPackEntry] = {}
        for pack_id in {item.id for item in self._entries}:
            candidate = self.latest(pack_id, include_yanked=include_yanked)
            if candidate is not None:
                latest_by_id[pack_id] = candidate

        results: list[AssetPackEntry] = []
        for item in latest_by_id.values():
            if type is not None and item.type != type:
                continue
            lowered_tags = {value.lower() for value in item.tags}
            if wanted_tag is not None and wanted_tag not in lowered_tags:
                continue
            lowered_permissions = {value.lower() for value in item.permissions}
            if wanted_permission is not None and wanted_permission not in lowered_permissions:
                continue
            if tokens:
                blob = " ".join(sorted(item.keywords())).lower()
                blob += " " + " ".join(
                    [
                        item.id,
                        item.name,
                        item.description,
                        item.author,
                        item.license,
                        *item.capabilities,
                        *item.permissions,
                    ]
                ).lower()
                if not all(token in blob for token in tokens):
                    continue
            results.append(item)

        results.sort(key=lambda item: (item.name.lower(), item.id))
        return results

    def categories(self) -> dict[str, Any]:
        entries = self.search(include_yanked=False)
        type_counts = Counter(item.type for item in entries)
        tag_counts: Counter[str] = Counter()
        permission_counts: Counter[str] = Counter()
        for item in entries:
            tag_counts.update(item.tags)
            permission_counts.update(item.permissions)
        categories = [
            {"id": key, "kind": "type", "count": count}
            for key, count in sorted(type_counts.items())
        ]
        categories.extend(
            {"id": key, "kind": "tag", "count": count}
            for key, count in sorted(tag_counts.items())
        )
        return {
            "types": dict(sorted(type_counts.items())),
            "tags": dict(sorted(tag_counts.items())),
            "permissions": dict(sorted(permission_counts.items())),
            "categories": categories,
        }

    def related(self, pack_id: str, *, limit: int = 10) -> list[AssetPackEntry]:
        target = self.entry(pack_id)
        if target is None:
            raise AssetNotFoundError(f"unknown pack {pack_id!r}")
        target_keywords = target.keywords()
        scored: list[tuple[int, str, AssetPackEntry]] = []
        for item in self.search(include_yanked=False):
            if item.id == pack_id:
                continue
            overlap = len(target_keywords & item.keywords())
            if overlap:
                scored.append((overlap, item.id, item))
        scored.sort(key=lambda value: (-value[0], value[1]))
        return [item for _, _, item in scored[: max(0, int(limit))]]

    def compute_hash(self) -> str:
        payload = sorted(
            (entry.to_manifest() for entry in self._entries),
            key=lambda item: (str(item.get("id", "")), str(item.get("version", ""))),
        )
        return _sha256(_canonical_json(payload))

    def stats(self) -> dict[str, Any]:
        type_counts = Counter(item.type for item in self._entries)
        tag_counts: Counter[str] = Counter()
        permission_counts: Counter[str] = Counter()
        total_bytes = 0
        for item in self._entries:
            tag_counts.update(item.tags)
            permission_counts.update(item.permissions)
            total_bytes += sum(max(0, asset.size) for asset in item.assets)
        return {
            "packs": len({item.id for item in self._entries}),
            "versions": len(self._entries),
            "total_packs": len({item.id for item in self._entries}),
            "total_versions": len(self._entries),
            "by_type": dict(sorted(type_counts.items())),
            "by_tag": dict(sorted(tag_counts.items())),
            "by_permission": dict(sorted(permission_counts.items())),
            "yanked_versions": sum(1 for item in self._entries if item.yanked),
            "total_bytes": total_bytes,
            "total_size_bytes": total_bytes,
            "last_sync": self.last_sync,
            "etag": self.etag,
            "content_hash": self.content_hash or self.compute_hash(),
        }

    @property
    def count(self) -> int:
        return len(self._entries)


# ── Cache ────────────────────────────────────────────────────────────────────


class AssetCache:
    """Filesystem cache for index, version snapshots, and local state."""

    def __init__(self, root: str | Path | None = None) -> None:
        self.root = Path(root) if root is not None else Path(__file__).parent / "asset_cache"
        self.index_path = self.root / "index.json"
        self.packs_dir = self.root / "packs"
        self.installed_path = self.root / "installed.json"
        self.submissions_path = self.root / "submissions.json"
        self._lock = threading.RLock()

    def ensure(self) -> None:
        self.root.mkdir(parents=True, exist_ok=True)

    def save_index(self, index: AssetIndex) -> None:
        self.ensure()
        index.save(self.index_path)

    def load_index(self) -> AssetIndex:
        return AssetIndex.from_file(self.index_path)

    def pack_dir(self, pack_id: str, version: str) -> Path:
        if not _ID_RE.match(pack_id or ""):
            raise InvalidAssetError(f"invalid pack id: {pack_id!r}")
        safe_version = _safe_relative_path(version)
        return self.packs_dir / pack_id / safe_version

    def store_pack_files(
        self,
        pack_id: str,
        version: str,
        files: Mapping[str, bytes],
        *,
        max_depth: int = 16,
    ) -> Path:
        destination = self.pack_dir(pack_id, version)
        with self._lock:
            _atomic_tree_replace(destination, files, max_depth=max_depth)
        return destination

    def cached_files(
        self,
        pack_id: str,
        version: str,
        *,
        max_depth: int = 16,
        limits: RepositoryLimits | None = None,
    ) -> dict[str, bytes] | None:
        directory = self.pack_dir(pack_id, version)
        if not directory.is_dir():
            return None
        bounds = limits or RepositoryLimits(max_depth=max_depth, max_files=10_000)
        return _collect_directory(directory, bounds)

    def load_installed(self) -> dict[str, Any]:
        if not self.installed_path.exists():
            return {"packs": {}}
        try:
            payload = json.loads(self.installed_path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return {"packs": {}}
        if not isinstance(payload, dict):
            return {"packs": {}}
        if not isinstance(payload.get("packs"), dict):
            payload["packs"] = {}
        return payload

    def record_installed(
        self,
        pack_id: str,
        version: str,
        *,
        previous: str | None = None,
    ) -> dict[str, Any]:
        with self._lock:
            state = self.load_installed()
            packs = state["packs"]
            record = dict(packs.get(pack_id) or {})
            current = record.get("current")
            history = list(record.get("history") or [])
            if previous is not None and previous != version:
                record["previous"] = previous
            elif current and current != version:
                record["previous"] = current
            record["current"] = version
            if version not in history:
                history.append(version)
            record["history"] = history
            packs[pack_id] = record
            self.ensure()
            _atomic_json_write(self.installed_path, state)
            return record

    def record_submission(
        self,
        pack_id: str,
        version: str,
        *,
        status: str = "queued",
    ) -> dict[str, Any]:
        with self._lock:
            if self.submissions_path.exists():
                try:
                    payload = json.loads(self.submissions_path.read_text(encoding="utf-8"))
                except (OSError, ValueError):
                    payload = {}
            else:
                payload = {}
            if not isinstance(payload, dict):
                payload = {}
            submissions = payload.get("submissions")
            if not isinstance(submissions, list):
                submissions = []
            record = {
                "id": pack_id,
                "version": version,
                "status": status,
                "submitted_at": _utc_now(),
            }
            submissions = [
                item
                for item in submissions
                if not (
                    isinstance(item, Mapping)
                    and item.get("id") == pack_id
                    and item.get("version") == version
                )
            ]
            submissions.append(record)
            payload["submissions"] = submissions
            self.ensure()
            _atomic_json_write(self.submissions_path, payload)
            return record


# ── Safe source collection ──────────────────────────────────────────────────


@dataclass(frozen=True)
class RepositoryLimits:
    """Bounds applied to local pack sources."""

    max_depth: int = 16
    max_files: int = 4096
    max_file_bytes: int = 32 * 1024 * 1024
    max_total_bytes: int = 64 * 1024 * 1024


def _collect_directory(root: Path, limits: RepositoryLimits) -> dict[str, bytes]:
    files: dict[str, bytes] = {}
    total = 0
    for dirpath, dirnames, filenames in os.walk(root, followlinks=False):
        current = Path(dirpath)
        relative = current.relative_to(root)
        if len(relative.parts) > limits.max_depth:
            raise UnsafeArchiveError(
                f"path depth exceeds {limits.max_depth}: {relative}"
            )
        for dirname in list(dirnames):
            child = current / dirname
            if child.is_symlink():
                raise UnsafeArchiveError(f"symlinked directory in pack: {child}")
            if len((relative / dirname).parts) > limits.max_depth:
                raise UnsafeArchiveError(
                    f"path depth exceeds {limits.max_depth}: {relative / dirname}"
                )
        for filename in filenames:
            path = current / filename
            if path.is_symlink():
                raise UnsafeArchiveError(f"symlink in pack: {path}")
            info = path.lstat()
            if not stat.S_ISREG(info.st_mode):
                raise UnsafeArchiveError(f"non-regular file in pack: {path}")
            if info.st_nlink > 1:
                raise UnsafeArchiveError(f"hardlink in pack: {path}")
            name = _safe_relative_path(
                path.relative_to(root).as_posix(), max_depth=limits.max_depth
            )
            if info.st_size > limits.max_file_bytes:
                raise UnsafeArchiveError(f"file exceeds size limit: {name!r}")
            data = path.read_bytes()
            if len(data) > limits.max_file_bytes:
                raise UnsafeArchiveError(f"file exceeds size limit: {name!r}")
            total += len(data)
            if total > limits.max_total_bytes:
                raise UnsafeArchiveError("pack exceeds total size limit")
            if len(files) >= limits.max_files:
                raise UnsafeArchiveError("pack exceeds file count limit")
            files[name] = data
    return files


def _read_zip(source: Any, limits: RepositoryLimits) -> dict[str, bytes]:
    files: dict[str, bytes] = {}
    total = 0
    with zipfile.ZipFile(source) as archive:
        infos = archive.infolist()
        if len(infos) > limits.max_files + 32:
            raise UnsafeArchiveError("archive exceeds file count limit")
        for info in infos:
            name = _safe_relative_path(info.filename, max_depth=limits.max_depth)
            if info.is_dir():
                continue
            mode = info.external_attr >> 16
            file_type = stat.S_IFMT(mode)
            if file_type:
                if file_type == stat.S_IFLNK:
                    raise UnsafeArchiveError(f"symlink in archive: {name!r}")
                if file_type != stat.S_IFREG:
                    raise UnsafeArchiveError(
                        f"non-regular file in archive: {name!r}"
                    )
            if name in files:
                raise UnsafeArchiveError(f"duplicate archive path: {name!r}")
            if info.file_size > limits.max_file_bytes:
                raise UnsafeArchiveError(f"file exceeds size limit: {name!r}")
            with archive.open(info) as handle:
                data = _read_limited(handle, limits.max_file_bytes, name)
            if len(data) > limits.max_file_bytes:
                raise UnsafeArchiveError(f"file exceeds size limit: {name!r}")
            total += len(data)
            if total > limits.max_total_bytes:
                raise UnsafeArchiveError("pack exceeds total size limit")
            if len(files) >= limits.max_files:
                raise UnsafeArchiveError("pack exceeds file count limit")
            files[name] = data
    return files


def _read_tar(source: Any, limits: RepositoryLimits) -> dict[str, bytes]:
    files: dict[str, bytes] = {}
    total = 0
    with tarfile.open(fileobj=source, mode="r:*") as archive:
        members = archive.getmembers()
        if len(members) > limits.max_files + 32:
            raise UnsafeArchiveError("archive exceeds file count limit")
        for member in members:
            name = _safe_relative_path(member.name, max_depth=limits.max_depth)
            if member.isdir():
                continue
            if not member.isfile():
                raise UnsafeArchiveError(
                    f"non-regular file in archive: {name!r}"
                )
            if name in files:
                raise UnsafeArchiveError(f"duplicate archive path: {name!r}")
            if member.size > limits.max_file_bytes:
                raise UnsafeArchiveError(f"file exceeds size limit: {name!r}")
            handle = archive.extractfile(member)
            if handle is None:
                raise UnsafeArchiveError(f"cannot read archive member: {name!r}")
            with handle:
                data = _read_limited(handle, limits.max_file_bytes, name)
            total += len(data)
            if total > limits.max_total_bytes:
                raise UnsafeArchiveError("pack exceeds total size limit")
            if len(files) >= limits.max_files:
                raise UnsafeArchiveError("pack exceeds file count limit")
            files[name] = data
    return files


def _collect_source(
    source: str | Path | bytes | bytearray,
    limits: RepositoryLimits,
) -> dict[str, bytes]:
    if isinstance(source, (bytes, bytearray)):
        data = bytes(source)
        buffer = io.BytesIO(data)
        if zipfile.is_zipfile(buffer):
            buffer.seek(0)
            return _read_zip(buffer, limits)
        buffer.seek(0)
        if tarfile.is_tarfile(buffer):
            buffer.seek(0)
            return _read_tar(buffer, limits)
        raise UnsafeArchiveError("source bytes are not a zip or tar archive")

    path = Path(source)
    if path.is_dir():
        return _collect_directory(path, limits)
    if not path.is_file():
        raise AssetNotFoundError(f"pack source not found: {path}")

    if zipfile.is_zipfile(path):
        with path.open("rb") as handle:
            return _read_zip(handle, limits)
    if tarfile.is_tarfile(path):
        with path.open("rb") as handle:
            return _read_tar(handle, limits)
    raise UnsafeArchiveError(f"not a pack directory or zip/tar archive: {path}")


def _normalize_manifest_root(
    files: Mapping[str, bytes],
    *,
    max_depth: int = 16,
) -> dict[str, bytes]:
    if "manifest.json" in files:
        base = ""
    else:
        candidates = [
            name for name in files if name.endswith("/manifest.json")
        ]
        if not candidates:
            raise InvalidAssetError("pack has no manifest.json")
        base = min(candidates, key=lambda value: value.count("/"))[: -len("manifest.json")]
        if sum(1 for value in candidates if value.startswith(base)) != 1:
            raise InvalidAssetError("pack contains multiple manifests")

    normalized: dict[str, bytes] = {}
    for raw_name, data in files.items():
        if base:
            if not raw_name.startswith(base):
                raise InvalidAssetError(
                    f"file outside pack root: {raw_name!r}"
                )
            name = raw_name[len(base):]
        else:
            name = raw_name
        normalized[_safe_relative_path(name, max_depth=max_depth)] = data
    if "manifest.json" not in normalized:
        raise InvalidAssetError("pack has no manifest.json")
    return normalized


def _parse_manifest(files: Mapping[str, bytes]) -> tuple[AssetPackEntry, bytes]:
    raw = files.get("manifest.json")
    if raw is None:
        raise InvalidAssetError("pack has no manifest.json")
    try:
        manifest = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, ValueError) as exc:
        raise InvalidAssetError(f"invalid manifest.json: {exc}") from exc
    if not isinstance(manifest, Mapping):
        raise InvalidAssetError("manifest.json must contain an object")
    return AssetPackEntry.from_manifest(manifest), raw


# ── Sync policy and transports ──────────────────────────────────────────────


@dataclass
class AssetSyncPolicy:
    """Controller sync policy mirroring the device-database precedent."""

    mode: str = "offline"
    sync_interval_hours: int = 24
    cache_size_mb: int | None = None
    include_previews: bool = True
    include_external_imports: bool = False
    verify_signatures: bool = False
    extra: dict[str, Any] = field(default_factory=dict)

    _KNOWN = frozenset({
        "mode",
        "sync_interval_hours",
        "cache_size_mb",
        "include_previews",
        "include_external_imports",
        "verify_signatures",
    })

    @classmethod
    def from_dict(cls, raw: Mapping[str, Any] | None = None) -> AssetSyncPolicy:
        data = _as_mapping(raw)
        mode = str(data.get("mode") or "offline").strip().lower()
        if mode not in SYNC_MODES:
            raise InvalidAssetError(f"unknown sync mode: {mode!r}")
        try:
            interval = int(data.get("sync_interval_hours", 24) or 24)
        except (TypeError, ValueError):
            raise InvalidAssetError("sync_interval_hours must be an integer") from None
        if interval < 0:
            raise InvalidAssetError("sync_interval_hours cannot be negative")
        cache_size = data.get("cache_size_mb")
        if cache_size is not None:
            try:
                cache_size = int(cache_size)
            except (TypeError, ValueError):
                raise InvalidAssetError("cache_size_mb must be an integer") from None
            if cache_size < 0:
                raise InvalidAssetError("cache_size_mb cannot be negative")
        return cls(
            mode=mode,
            sync_interval_hours=interval,
            cache_size_mb=cache_size,
            include_previews=_as_bool(data.get("include_previews"), True),
            include_external_imports=_as_bool(
                data.get("include_external_imports"), False
            ),
            verify_signatures=_as_bool(data.get("verify_signatures"), False),
            extra={k: v for k, v in data.items() if k not in cls._KNOWN},
        )

    def to_dict(self) -> dict[str, Any]:
        data = dict(self.extra)
        data.update({
            "mode": self.mode,
            "sync_interval_hours": self.sync_interval_hours,
            "cache_size_mb": self.cache_size_mb,
            "include_previews": self.include_previews,
            "include_external_imports": self.include_external_imports,
            "verify_signatures": self.verify_signatures,
        })
        return data

    def sync_due(self, last_sync: str | None, *, now: datetime | None = None) -> bool:
        if self.mode == "offline":
            return False
        if not last_sync:
            return True
        try:
            previous = datetime.fromisoformat(str(last_sync))
        except ValueError:
            return True
        if previous.tzinfo is None:
            previous = previous.replace(tzinfo=timezone.utc)
        current = now or datetime.now(timezone.utc)
        return (current - previous).total_seconds() >= self.sync_interval_hours * 3600


@dataclass
class _TransportResponse:
    status_code: int = 200
    headers: Mapping[str, str] = field(default_factory=dict)
    content: bytes = b""


def _urllib_transport(url: str, headers: Mapping[str, str] | None = None) -> _TransportResponse:
    request = Request(url, headers=dict(headers or {}))
    try:
        with urlopen(request, timeout=30) as response:  # noqa: S310 - explicit sync URL
            return _TransportResponse(
                status_code=int(getattr(response, "status", 200)),
                headers=dict(response.headers.items()),
                content=response.read(),
            )
    except HTTPError as exc:
        if exc.code == 304:
            return _TransportResponse(
                status_code=304,
                headers=dict((exc.headers or {}).items()),
                content=b"",
            )
        raise AssetUpstreamError(f"index request failed: HTTP {exc.code}") from exc
    except URLError as exc:
        raise AssetUpstreamError(f"index request failed: {exc.reason}") from exc


def _call_transport(
    transport: Callable[..., Any],
    url: str,
    headers: Mapping[str, str],
) -> Any:
    try:
        return transport(url, headers=headers)
    except TypeError:
        return transport(url)


def _response_status(response: Any) -> int:
    if isinstance(response, _TransportResponse):
        return response.status_code
    value = getattr(response, "status_code", None)
    if value is None:
        value = getattr(response, "status", None)
    try:
        return int(value) if value is not None else 200
    except (TypeError, ValueError):
        return 200


def _response_headers(response: Any) -> Mapping[str, str]:
    headers = getattr(response, "headers", None)
    if isinstance(headers, Mapping):
        return headers
    return {}


def _response_body(response: Any) -> bytes:
    if isinstance(response, (bytes, bytearray)):
        return bytes(response)
    if isinstance(response, str):
        return response.encode("utf-8")
    if isinstance(response, (dict, list)):
        return _canonical_json(response)
    content = getattr(response, "content", None)
    if content is not None:
        return content.encode("utf-8") if isinstance(content, str) else bytes(content)
    text = getattr(response, "text", None)
    if text is not None:
        return text.encode("utf-8") if isinstance(text, str) else bytes(text)
    payload = response.json()
    return _canonical_json(payload)


def _response_payload(response: Any) -> tuple[Any, bytes]:
    if isinstance(response, (dict, list)):
        return response, _canonical_json(response)
    raw = _response_body(response)
    try:
        return json.loads(raw.decode("utf-8")), raw
    except (UnicodeDecodeError, ValueError) as exc:
        if hasattr(response, "json"):
            payload = response.json()
            return payload, _canonical_json(payload)
        raise InvalidAssetError(f"index response is not JSON: {exc}") from exc


# ── Repository ──────────────────────────────────────────────────────────────


class AssetRepository:
    """Index + cache + atomic install/update/rollback + sync."""

    def __init__(
        self,
        packs_dir: str | Path | None = None,
        cache_dir: str | Path | None = None,
        *,
        cache: AssetCache | None = None,
        index: AssetIndex | None = None,
        policy: AssetSyncPolicy | Mapping[str, Any] | None = None,
        signature_verifier: Callable[[bytes, str, Mapping[str, bytes]], bool] | None = None,
        allow_unsigned: bool = True,
        require_signature: bool = False,
        allowed_licenses: Sequence[str] | None = None,
        enforce_license: bool = False,
        strict_checksums: bool = True,
        limits: RepositoryLimits | None = None,
        transport: Callable[..., Any] | None = None,
        index_url: str | None = None,
    ) -> None:
        self.packs_dir = (
            Path(packs_dir) if packs_dir is not None else Path(__file__).parent / "widget_packs"
        )
        self.cache = cache or AssetCache(cache_dir)
        self.signature_verifier = signature_verifier
        self.allow_unsigned = bool(allow_unsigned)
        self.allowed_licenses = frozenset(
            allowed_licenses if allowed_licenses is not None else DEFAULT_ALLOWED_LICENSES
        )
        self.enforce_license = bool(enforce_license)
        self.strict_checksums = bool(strict_checksums)
        self.limits = limits or RepositoryLimits()
        self.transport = transport
        self.index_url = index_url
        if isinstance(policy, AssetSyncPolicy):
            self.policy = policy
        else:
            self.policy = AssetSyncPolicy.from_dict(policy)
        self.require_signature = bool(
            require_signature or self.policy.verify_signatures
        )
        self._lock = threading.RLock()

        if index is not None:
            self.index = index
            if self.index.path is None:
                self.index.path = self.cache.index_path
        elif self.cache.index_path.exists():
            self.index = AssetIndex.from_file(self.cache.index_path)
        else:
            self.index = AssetIndex(path=self.cache.index_path)

    # Index helpers -------------------------------------------------------

    def save_index(self) -> None:
        with self._lock:
            self.cache.save_index(self.index)

    def search(self, *args: Any, **kwargs: Any) -> list[AssetPackEntry]:
        return self.index.search(*args, **kwargs)

    def categories(self) -> dict[str, Any]:
        return self.index.categories()

    def related(self, pack_id: str, *, limit: int = 10) -> list[AssetPackEntry]:
        return self.index.related(pack_id, limit=limit)

    def stats(self) -> dict[str, Any]:
        data = self.index.stats()
        data["installed_packs"] = len(self.installed_ids())
        data["policy_mode"] = self.policy.mode
        data["cache_dir"] = str(self.cache.root)
        data["packs_dir"] = str(self.packs_dir)
        return data

    def installed_ids(self) -> set[str]:
        found = set(self.cache.load_installed().get("packs", {}).keys())
        if self.packs_dir.is_dir():
            for child in self.packs_dir.iterdir():
                if child.is_dir() and not child.name.startswith("."):
                    if (child / "manifest.json").is_file():
                        found.add(child.name)
        return found

    def installed_version(self, pack_id: str) -> str | None:
        if not _ID_RE.match(pack_id or ""):
            raise InvalidAssetError(f"invalid pack id: {pack_id!r}")
        manifest_path = self.packs_dir / pack_id / "manifest.json"
        if manifest_path.is_file():
            try:
                manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
                version = str(manifest.get("version") or "")
                if version:
                    return version
            except (OSError, ValueError):
                pass
        record = self.cache.load_installed().get("packs", {}).get(pack_id) or {}
        current = record.get("current")
        return str(current) if current else None

    # Verification --------------------------------------------------------

    def _verify_signature(
        self,
        entry: AssetPackEntry,
        manifest_bytes: bytes,
        files: Mapping[str, bytes],
    ) -> None:
        signature = (entry.signature or "").strip()
        if not signature:
            if self.require_signature:
                raise AssetSignatureError("pack is unsigned and signatures are required")
            if not self.allow_unsigned:
                raise AssetSignatureError("unsigned packs are not allowed")
            return
        if self.signature_verifier is None:
            raise AssetSignatureError(
                "pack carries a signature but no verifier is configured"
            )
        try:
            valid = _invoke_verifier(
                self.signature_verifier, manifest_bytes, signature, files
            )
        except Exception as exc:  # verifier failures fail closed
            raise AssetSignatureError(f"signature verification failed: {exc}") from exc
        if not valid:
            raise AssetSignatureError("signature verification failed")

    def _verify_assets(self, entry: AssetPackEntry, files: Mapping[str, bytes]) -> None:
        expected: dict[str, str] = {}
        seen_assets: set[str] = set()

        def add_expected(name: str, digest: str, source: str) -> None:
            existing = expected.get(name)
            normalized = digest.strip().lower()
            if existing is not None and existing != normalized:
                raise InvalidAssetError(
                    f"conflicting checksums for {name!r} in {source}"
                )
            expected[name] = normalized

        for asset in entry.assets:
            name = _safe_relative_path(asset.path, max_depth=self.limits.max_depth)
            if name in seen_assets:
                raise InvalidAssetError(f"duplicate asset declaration: {name!r}")
            seen_assets.add(name)
            if not asset.sha256:
                raise AssetHashError(f"asset has no SHA-256: {name!r}")
            add_expected(name, asset.sha256, "assets")
            if name not in files:
                raise InvalidAssetError(f"declared asset missing from pack: {name!r}")
            if asset.size and asset.size != len(files[name]):
                raise AssetHashError(f"size mismatch for {name!r}")

        inline_checksums = entry.extra.get("checksums")
        if isinstance(inline_checksums, (Mapping, list)):
            for raw_name, digest in _extract_checksums(inline_checksums).items():
                add_expected(
                    _safe_relative_path(raw_name, max_depth=self.limits.max_depth),
                    digest,
                    "manifest checksums",
                )

        raw_checksum_file = files.get("checksums.json")
        if raw_checksum_file:
            try:
                checksum_payload = json.loads(raw_checksum_file.decode("utf-8"))
            except (UnicodeDecodeError, ValueError) as exc:
                raise InvalidAssetError(f"invalid checksums.json: {exc}") from exc
            for raw_name, digest in _extract_checksums(checksum_payload).items():
                add_expected(
                    _safe_relative_path(raw_name, max_depth=self.limits.max_depth),
                    digest,
                    "checksums.json",
                )

        for name, digest in expected.items():
            if name not in files:
                raise InvalidAssetError(f"declared asset missing from pack: {name!r}")
            if _sha256(files[name]).lower() != digest:
                raise AssetHashError(f"SHA-256 mismatch for {name!r}")

        if expected and self.strict_checksums:
            for name in files:
                if name in expected:
                    continue
                if Path(name).name in _METADATA_FILENAMES:
                    continue
                raise InvalidAssetError(f"undeclared pack file: {name!r}")

    def _check_license(self, entry: AssetPackEntry) -> None:
        if not license_is_allowed(entry.license, self.allowed_licenses):
            raise InvalidAssetError(
                f"licence {entry.license or '<missing>'!r} is not on the publish allowlist"
            )

    # Install / update / rollback ----------------------------------------

    def install(
        self,
        source: str | Path | bytes | bytearray | None = None,
        *,
        version: str | None = None,
        auto: bool = False,
        pack_id: str | None = None,
    ) -> AssetPackEntry:
        """Install a local pack directory or zip/tar archive atomically.

        Passing only ``pack_id=`` (or a string that names an indexed pack with
        no matching local path) delegates to :meth:`install_from_index`.
        """
        if source is None:
            if pack_id is None:
                raise InvalidAssetError("install requires a source or pack_id")
            return self.install_from_index(pack_id, version, auto=auto)
        with self._lock:
            files = _collect_source(source, self.limits)
            files = _normalize_manifest_root(files, max_depth=self.limits.max_depth)
            entry, manifest_bytes = _parse_manifest(files)
            if version is not None and entry.version != version:
                raise InvalidAssetError(
                    f"manifest version {entry.version!r} does not match requested {version!r}"
                )
            if auto and entry.yanked:
                raise AssetConflictError(
                    f"pack {entry.id!r} version {entry.version!r} is yanked"
                )
            if self.enforce_license:
                self._check_license(entry)
            self._verify_signature(entry, manifest_bytes, files)
            self._verify_assets(entry, files)

            previous = self.installed_version(entry.id)
            self._atomic_install(entry, files)
            snapshot = {name: data for name, data in files.items() if name != "options.json"}
            self.cache.store_pack_files(
                entry.id,
                entry.version,
                snapshot,
                max_depth=self.limits.max_depth,
            )
            self.cache.record_installed(entry.id, entry.version, previous=previous)
            log.info("Installed asset pack %s v%s", entry.id, entry.version)
            return entry

    def update(
        self,
        source: str | Path | bytes | bytearray | None = None,
        *,
        version: str | None = None,
        auto: bool = False,
        pack_id: str | None = None,
    ) -> AssetPackEntry:
        """Update an installed pack; the previous version stays cached.

        ``update("pack-id")`` resolves the requested version from the index,
        while a path/archive installs and caches a local payload.
        """
        if source is None:
            if pack_id is None:
                raise InvalidAssetError("update requires a source or pack_id")
            return self.install_from_index(pack_id, version, auto=auto)
        if (
            isinstance(source, str)
            and not Path(source).exists()
            and self.index.entry(source) is not None
        ):
            return self.install_from_index(source, version, auto=auto)
        return self.install(source, version=version, auto=auto)

    def _atomic_install(self, entry: AssetPackEntry, files: Mapping[str, bytes]) -> None:
        destination = self.packs_dir / entry.id
        self.packs_dir.mkdir(parents=True, exist_ok=True)
        staging = Path(
            tempfile.mkdtemp(
                prefix=f".{entry.id}.tmp-", dir=str(self.packs_dir)
            )
        )
        backup: Path | None = None
        try:
            existing_options: bytes | None = None
            if destination.exists():
                if destination.is_symlink():
                    raise UnsafeArchiveError(
                        f"refusing to replace symlinked pack directory: {destination}"
                    )
                options_path = destination / "options.json"
                if options_path.is_file():
                    existing_options = options_path.read_bytes()

            for raw_name, data in files.items():
                name = _safe_relative_path(raw_name, max_depth=self.limits.max_depth)
                if name == "options.json" and existing_options is not None:
                    continue
                target = staging.joinpath(*name.split("/"))
                target.parent.mkdir(parents=True, exist_ok=True)
                with open(target, "wb") as handle:
                    handle.write(data)

            if existing_options is not None:
                (staging / "options.json").write_bytes(existing_options)

            if destination.exists():
                backup = self.packs_dir / f".{entry.id}.old-{uuid.uuid4().hex}"
                os.replace(destination, backup)
            try:
                os.replace(staging, destination)
            except Exception:
                if backup is not None and not destination.exists():
                    os.replace(backup, destination)
                raise
            if backup is not None:
                shutil.rmtree(backup, ignore_errors=True)
        finally:
            if staging.exists():
                shutil.rmtree(staging, ignore_errors=True)

    def rollback(self, pack_id: str, version: str | None = None) -> AssetPackEntry:
        """Restore a cached version, defaulting to the previous install."""
        with self._lock:
            state = self.cache.load_installed().get("packs", {})
            record = state.get(pack_id) or {}
            current = self.installed_version(pack_id)
            target = version or record.get("previous")
            if not target:
                raise AssetConflictError(f"no previous version for pack {pack_id!r}")
            target = str(target)
            if current is not None and target == current:
                raise AssetConflictError(
                    f"pack {pack_id!r} is already at version {target!r}"
                )
            cached = self.cache.cached_files(
                pack_id,
                target,
                max_depth=self.limits.max_depth,
                limits=self.limits,
            )
            if cached is None:
                raise AssetNotFoundError(
                    f"no cached payload for {pack_id!r} version {target!r}"
                )
            files = _normalize_manifest_root(
                cached, max_depth=self.limits.max_depth
            )
            entry, _ = _parse_manifest(files)
            if entry.version != target:
                raise InvalidAssetError(
                    f"cached manifest version {entry.version!r} does not match {target!r}"
                )
            self._verify_assets(entry, files)
            self._atomic_install(entry, files)
            self.cache.record_installed(pack_id, entry.version, previous=current)
            log.info("Rolled back asset pack %s to v%s", pack_id, entry.version)
            return entry

    def install_from_index(
        self,
        pack_id: str,
        version: str | None = None,
        *,
        transport: Callable[..., Any] | None = None,
        auto: bool = False,
    ) -> AssetPackEntry:
        """Install an indexed version from cache or its ``download_url``."""
        with self._lock:
            entry = self.index.entry(pack_id, version)
            if entry is None:
                raise AssetNotFoundError(f"unknown pack {pack_id!r}")
            if version is not None and entry.version != version:
                raise AssetNotFoundError(
                    f"unknown version {version!r} for pack {pack_id!r}"
                )
            return self._install_entry(entry, transport=transport, auto=auto)

    def _install_entry(
        self,
        entry: AssetPackEntry,
        *,
        transport: Callable[..., Any] | None,
        auto: bool,
    ) -> AssetPackEntry:
        if auto and entry.yanked:
            raise AssetConflictError(
                f"pack {entry.id!r} version {entry.version!r} is yanked"
            )
        cached = self.cache.cached_files(
            entry.id,
            entry.version,
            max_depth=self.limits.max_depth,
            limits=self.limits,
        )
        if cached is not None:
            files = _normalize_manifest_root(
                cached, max_depth=self.limits.max_depth
            )
            cached_entry, _ = _parse_manifest(files)
            self._verify_assets(cached_entry, files)
            previous = self.installed_version(entry.id)
            self._atomic_install(cached_entry, files)
            self.cache.record_installed(
                entry.id, cached_entry.version, previous=previous
            )
            return cached_entry

        download_url = (
            entry.extra.get("download_url")
            or entry.extra.get("archive_url")
            or entry.extra.get("url")
        )
        if not download_url:
            raise AssetConflictError(
                f"no cached payload or download_url for {entry.id!r} v{entry.version}"
            )
        if self.policy.mode == "offline":
            raise AssetConflictError(
                f"offline mode: no cached payload for {entry.id!r} v{entry.version}"
            )
        try:
            response = _call_transport(
                transport or self.transport or _urllib_transport,
                str(download_url),
                {},
            )
        except AssetRepositoryError:
            raise
        except Exception as exc:
            raise AssetUpstreamError(f"pack download failed: {exc}") from exc
        status = _response_status(response)
        if status >= 400:
            raise AssetUpstreamError(
                f"pack download failed: HTTP {status}"
            )
        data = _response_body(response)
        return self.install(data, version=entry.version, auto=auto)

    # Sync ----------------------------------------------------------------

    def sync(
        self,
        index_url: str | None = None,
        transport: Callable[..., Any] | None = None,
        *,
        policy: AssetSyncPolicy | Mapping[str, Any] | None = None,
    ) -> dict[str, Any]:
        """Fetch and verify the remote index, or return the offline cache.

        ``offline`` never calls the transport.  The local index is replaced only
        after JSON parsing and signature verification succeed.
        """
        active = (
            policy
            if isinstance(policy, AssetSyncPolicy)
            else AssetSyncPolicy.from_dict(policy)
            if policy is not None
            else self.policy
        )
        if active.mode == "offline":
            return self._sync_result(
                active,
                synced=False,
                offline=True,
                cached=True,
                message="offline mode: cache used without network access",
            )

        url = index_url or self.index_url or self.index.extra.get("index_url")
        if not url:
            raise InvalidAssetError("no index URL configured for sync")

        headers: dict[str, str] = {}
        if self.index.etag:
            headers["If-None-Match"] = self.index.etag
        try:
            response = _call_transport(
                transport or self.transport or _urllib_transport,
                str(url),
                headers,
            )
        except AssetRepositoryError:
            raise
        except Exception as exc:
            raise AssetUpstreamError(f"index request failed: {exc}") from exc
        status = _response_status(response)
        if status == 304:
            return self._sync_result(
                active,
                synced=False,
                offline=False,
                cached=True,
                not_modified=True,
                message="index not modified",
            )
        if status >= 400:
            raise AssetUpstreamError(f"index request failed: HTTP {status}")

        payload, raw = _response_payload(response)
        signature = ""
        signed_payload = raw
        if isinstance(payload, Mapping):
            raw_signature = payload.get("signature")
            if isinstance(raw_signature, Mapping):
                # Connect indexes sign canonical JSON without the signature
                # envelope; the controller must verify that same payload.
                signature = str(raw_signature.get("value") or "")
                unsigned = {
                    key: value for key, value in payload.items() if key != "signature"
                }
                signed_payload = _canonical_json(unsigned)
            else:
                signature = str(raw_signature or "")
        if signature:
            if self.signature_verifier is None:
                raise AssetSignatureError(
                    "index carries a signature but no verifier is configured"
                )
            try:
                valid = _invoke_verifier(
                    self.signature_verifier, signed_payload, signature, {}
                )
            except Exception as exc:
                raise AssetSignatureError(
                    f"index signature verification failed: {exc}"
                ) from exc
            if not valid:
                raise AssetSignatureError("index signature verification failed")
        elif active.verify_signatures or not self.allow_unsigned:
            raise AssetSignatureError("index is unsigned and signatures are required")

        candidate = AssetIndex(path=self.cache.index_path)
        candidate._apply_payload(payload)
        if not active.include_external_imports:
            candidate._entries = [
                item for item in candidate._entries if item.type != "import_pack"
            ]
        if active.mode == "installed":
            installed = self.installed_ids()
            candidate._entries = [
                item for item in candidate._entries if item.id in installed
            ]
        candidate.last_sync = _utc_now()
        headers_lower = {str(key).lower(): value for key, value in _response_headers(response).items()}
        candidate.etag = str(
            headers_lower.get("etag")
            or (payload.get("etag") if isinstance(payload, Mapping) else "")
            or _etag_for(raw)
        )
        candidate.signature = signature
        candidate.content_hash = candidate.compute_hash()

        with self._lock:
            self.cache.ensure()
            candidate.save(self.cache.index_path)
            self.index = candidate
            self.policy = active

        return self._sync_result(
            active,
            synced=True,
            offline=False,
            cached=False,
            updated=True,
            message="index updated",
        )

    def _sync_result(self, policy: AssetSyncPolicy, **updates: Any) -> dict[str, Any]:
        result: dict[str, Any] = {
            "mode": policy.mode,
            "sync_interval_hours": policy.sync_interval_hours,
            "cache": str(self.cache.root),
            "entries": self.index.count,
            "packs": self.index.stats()["packs"],
            "last_sync": self.index.last_sync,
            "etag": self.index.etag,
            "content_hash": self.index.content_hash,
        }
        result.update(updates)
        return result

    # Submission ----------------------------------------------------------

    def submit(
        self,
        pack_id: str,
        version: str | None = None,
        *,
        attestation: bool = False,
    ) -> dict[str, Any]:
        entry = self.index.entry(pack_id, version)
        if entry is None:
            raise AssetNotFoundError(f"unknown pack {pack_id!r}")
        if entry.yanked:
            raise AssetConflictError(
                f"pack {entry.id!r} version {entry.version!r} is yanked"
            )
        if not license_is_allowed(entry.license, self.allowed_licenses):
            raise InvalidAssetError(
                f"licence {entry.license or '<missing>'!r} is not on the publish allowlist"
            )
        private = bool(entry.extra.get("private", False)) or entry.type == "import_pack"
        if private and not attestation:
            raise AssetConflictError(
                "private/import packs require explicit submission attestation"
            )
        record = self.cache.record_submission(entry.id, entry.version)
        return {
            "ok": True,
            "id": entry.id,
            "version": entry.version,
            "status": record["status"],
            "submitted_at": record["submitted_at"],
        }


# ── API router ──────────────────────────────────────────────────────────────


def _http_error(exc: AssetRepositoryError) -> HTTPException:
    return HTTPException(
        status_code=exc.status_code,
        detail={"code": exc.code, "message": str(exc)},
    )


def build_router(manager: AssetRepository) -> APIRouter:
    """Build the ``/api/v1/asset-db`` router around a repository instance."""
    router = APIRouter(tags=["asset-db"])

    @router.get("/api/v1/asset-db/search")
    def search_assets(
        q: str | None = None,
        type: str | None = None,
        tag: str | None = None,
        permission: str | None = None,
        include_yanked: bool = False,
    ) -> dict[str, Any]:
        try:
            entries = manager.search(
                q,
                type=type,
                tag=tag,
                permission=permission,
                include_yanked=include_yanked,
            )
        except AssetRepositoryError as exc:
            raise _http_error(exc) from exc
        return {
            "entries": [entry.to_manifest() for entry in entries],
            "count": len(entries),
            "query": q,
        }

    @router.get("/api/v1/asset-db/entry/{pack_id}")
    def get_entry(pack_id: str, version: str | None = None) -> dict[str, Any]:
        entry = manager.index.entry(pack_id, version)
        if entry is None:
            raise _http_error(
                AssetNotFoundError(f"unknown pack {pack_id!r}")
            )
        return entry.to_manifest()

    @router.get("/api/v1/asset-db/versions/{pack_id}")
    def get_versions(
        pack_id: str,
        include_yanked: bool = True,
    ) -> dict[str, Any]:
        versions = manager.index.versions(pack_id, include_yanked=include_yanked)
        if not versions:
            raise _http_error(
                AssetNotFoundError(f"unknown pack {pack_id!r}")
            )
        return {
            "id": pack_id,
            "versions": [entry.to_manifest() for entry in versions],
            "count": len(versions),
        }

    @router.get("/api/v1/asset-db/categories")
    def get_categories() -> dict[str, Any]:
        return manager.categories()

    @router.post("/api/v1/asset-db/install/{pack_id}")
    def install_pack(
        pack_id: str,
        version: str | None = None,
        auto: bool = False,
    ) -> dict[str, Any]:
        try:
            entry = manager.install_from_index(pack_id, version, auto=auto)
        except AssetRepositoryError as exc:
            raise _http_error(exc) from exc
        return {
            "ok": True,
            "id": entry.id,
            "version": entry.version,
            "pack": entry.to_manifest(),
        }

    @router.post("/api/v1/asset-db/sync")
    def sync_assets(
        payload: dict[str, Any] | None = Body(default=None),
        index_url: str | None = Query(default=None),
    ) -> dict[str, Any]:
        body = payload or {}
        try:
            policy = manager.policy
            if body.get("mode"):
                merged = policy.to_dict()
                merged["mode"] = body["mode"]
                policy = AssetSyncPolicy.from_dict(merged)
            result = manager.sync(
                body.get("index_url") or index_url,
                policy=policy,
            )
        except AssetRepositoryError as exc:
            raise _http_error(exc) from exc
        return result

    @router.post("/api/v1/asset-db/submit/{pack_id}")
    def submit_pack(
        pack_id: str,
        version: str | None = None,
        payload: dict[str, Any] | None = Body(default=None),
    ) -> dict[str, Any]:
        body = payload or {}
        try:
            result = manager.submit(
                pack_id,
                version,
                attestation=bool(body.get("attestation", False)),
            )
        except AssetRepositoryError as exc:
            raise _http_error(exc) from exc
        return result

    @router.get("/api/v1/asset-db/stats")
    def get_stats() -> dict[str, Any]:
        return manager.stats()

    return router


__all__ = [
    "ASSET_SCHEMA",
    "INDEX_SCHEMA",
    "PACK_TYPES",
    "SYNC_MODES",
    "DEFAULT_ALLOWED_LICENSES",
    "AssetCache",
    "AssetConflictError",
    "AssetFile",
    "AssetHashError",
    "AssetIndex",
    "AssetNotFoundError",
    "AssetPackEntry",
    "AssetRepository",
    "AssetRepositoryError",
    "AssetSignatureError",
    "AssetSyncPolicy",
    "AssetUpstreamError",
    "InvalidAssetError",
    "RepositoryLimits",
    "UnsafeArchiveError",
    "build_router",
    "license_is_allowed",
]
