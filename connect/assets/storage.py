"""Content-addressed object storage for shared asset payloads.

The abstraction is deliberately tiny and network-free.  Production can map it
onto S3-compatible object storage; tests use an in-memory implementation.  All
implementations validate keys before touching a backing store so a bad manifest
or index can never escape the configured prefix.

Payloads are addressed by their SHA-256 digest.  Use :func:`content_key` or
:func:`put_content` for new payloads; the explicit ``put`` method exists for
migrations and operator tooling.
"""

from __future__ import annotations

import hashlib
import os
import tempfile
from pathlib import Path, PurePosixPath
from typing import Protocol, runtime_checkable

from connect.assets.models import (
    AssetError,
    InvalidAssetError,
    StorageError,
    StorageLimitError,
    StorageNotFoundError,
    UnsafeStorageKey,
)

DEFAULT_MAX_BYTES = 64 * 1024 * 1024


def sha256_hex(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def validate_key(key: str) -> str:
    """Validate a logical object key and return it unchanged.

    Absolute paths, parent traversal, backslashes, NUL bytes, empty keys and
    empty path segments are rejected.  Keys always use forward slashes.
    """
    if not isinstance(key, str) or not key:
        raise UnsafeStorageKey("object key must be a non-empty string")
    if "\x00" in key:
        raise UnsafeStorageKey("object key contains a NUL byte")
    if "\\" in key:
        raise UnsafeStorageKey("object key must use forward slashes")
    if key.startswith("/") or key.startswith("~"):
        raise UnsafeStorageKey("object key must be relative")
    if len(key) > 1024:
        raise UnsafeStorageKey("object key is too long")
    path = PurePosixPath(key)
    if path.is_absolute():
        raise UnsafeStorageKey("object key must be relative")
    parts = key.split("/")
    if any(part in {"", ".", ".."} for part in parts):
        raise UnsafeStorageKey("object key contains an unsafe path segment")
    if os.path.isabs(key) or (len(key) > 1 and key[1] == ":"):
        raise UnsafeStorageKey("object key must be relative")
    return key


def content_key(
    data: bytes,
    *,
    prefix: str = "sha256",
    suffix: str = "",
) -> str:
    """Return the content-addressed key for ``data``."""
    digest = sha256_hex(data)
    key = f"{prefix}/{digest[:2]}/{digest[2:]}{suffix}"
    return validate_key(key)


@runtime_checkable
class ObjectStore(Protocol):
    """Minimal object-store contract used by the shared asset service."""

    def put(
        self,
        key: str,
        data: bytes,
        *,
        content_type: str | None = None,
    ) -> str:
        ...

    def get(self, key: str) -> bytes:
        ...

    def exists(self, key: str) -> bool:
        ...

    def delete(self, key: str) -> bool:
        ...

    def signed_url(self, key: str, *, expires_in: int = 3600) -> str:
        ...


class _BaseStore:
    def __init__(self, *, max_bytes: int = DEFAULT_MAX_BYTES) -> None:
        if max_bytes <= 0:
            raise InvalidAssetError("max_bytes must be positive")
        self.max_bytes = int(max_bytes)

    def _prepare(self, key: str, data: bytes) -> tuple[str, bytes]:
        validated = validate_key(key)
        if not isinstance(data, (bytes, bytearray, memoryview)):
            raise InvalidAssetError("object payload must be bytes")
        payload = bytes(data)
        if len(payload) > self.max_bytes:
            raise StorageLimitError(
                f"object payload exceeds the {self.max_bytes}-byte limit"
            )
        return validated, payload

    def put_content(
        self,
        data: bytes,
        *,
        prefix: str = "sha256",
        suffix: str = "",
        content_type: str | None = None,
    ) -> str:
        """Store ``data`` under its SHA-256 key and return that key."""
        key = content_key(data, prefix=prefix, suffix=suffix)
        return self.put(key, data, content_type=content_type)


class MemoryStore(_BaseStore):
    """A process-local object store used by unit tests."""

    def __init__(self, *, max_bytes: int = DEFAULT_MAX_BYTES) -> None:
        super().__init__(max_bytes=max_bytes)
        self._objects: dict[str, bytes] = {}
        self._content_types: dict[str, str] = {}

    def put(
        self,
        key: str | bytes,
        data: bytes | None = None,
        *,
        content_type: str | None = None,
    ) -> str:
        if data is None:
            if not isinstance(key, (bytes, bytearray, memoryview)):
                raise InvalidAssetError(
                    "put requires a key and payload, or payload bytes"
                )
            data = bytes(key)
            key = content_key(data)
        validated, payload = self._prepare(key, data)
        self._objects[validated] = payload
        if content_type:
            self._content_types[validated] = content_type
        return validated

    def get(self, key: str) -> bytes:
        validated = validate_key(key)
        try:
            return self._objects[validated]
        except KeyError:
            raise StorageNotFoundError(f"unknown object {validated!r}") from None

    def exists(self, key: str) -> bool:
        return validate_key(key) in self._objects

    def delete(self, key: str) -> bool:
        validated = validate_key(key)
        self._content_types.pop(validated, None)
        return self._objects.pop(validated, None) is not None

    def signed_url(self, key: str, *, expires_in: int = 3600) -> str:
        validated = validate_key(key)
        return f"memory://{validated}"


class FileStore(_BaseStore):
    """A filesystem-backed store rooted at a single directory.

    Writes are atomic (temp file in the destination directory + ``os.replace``)
    and keys are resolved beneath ``root`` before any operation.
    """

    def __init__(
        self,
        root: str | Path,
        *,
        max_bytes: int = DEFAULT_MAX_BYTES,
    ) -> None:
        super().__init__(max_bytes=max_bytes)
        self.root = Path(root).expanduser().resolve()
        self.root.mkdir(parents=True, exist_ok=True)

    def _path(self, key: str) -> Path:
        validated = validate_key(key)
        candidate = (self.root / validated).resolve()
        try:
            candidate.relative_to(self.root)
        except ValueError:
            raise UnsafeStorageKey("object key escapes the store root") from None
        return candidate

    def put(
        self,
        key: str | bytes,
        data: bytes | None = None,
        *,
        content_type: str | None = None,
    ) -> str:
        if data is None:
            if not isinstance(key, (bytes, bytearray, memoryview)):
                raise InvalidAssetError(
                    "put requires a key and payload, or payload bytes"
                )
            data = bytes(key)
            key = content_key(data)
        validated, payload = self._prepare(key, data)
        path = self._path(validated)
        path.parent.mkdir(parents=True, exist_ok=True)
        handle, temp_name = tempfile.mkstemp(
            prefix=".asset-",
            dir=str(path.parent),
        )
        try:
            with os.fdopen(handle, "wb") as stream:
                stream.write(payload)
                stream.flush()
                os.fsync(stream.fileno())
            os.replace(temp_name, path)
        except BaseException:
            try:
                os.unlink(temp_name)
            except OSError:
                pass
            raise
        return validated

    def get(self, key: str) -> bytes:
        path = self._path(key)
        try:
            return path.read_bytes()
        except FileNotFoundError:
            raise StorageNotFoundError(f"unknown object {key!r}") from None

    def exists(self, key: str) -> bool:
        return self._path(key).is_file()

    def delete(self, key: str) -> bool:
        path = self._path(key)
        try:
            path.unlink()
            return True
        except FileNotFoundError:
            return False

    def signed_url(self, key: str, *, expires_in: int = 3600) -> str:
        path = self._path(key)
        return path.as_uri()


def put_content(
    store: ObjectStore,
    data: bytes,
    *,
    prefix: str = "sha256",
    suffix: str = "",
    content_type: str | None = None,
) -> str:
    """Store content-addressed bytes with any :class:`ObjectStore`."""
    key = content_key(data, prefix=prefix, suffix=suffix)
    store.put(key, data, content_type=content_type)
    return key


__all__ = [
    "DEFAULT_MAX_BYTES",
    "FileStore",
    "MemoryStore",
    "ObjectStore",
    "AssetError",
    "StorageError",
    "content_key",
    "put_content",
    "sha256_hex",
    "validate_key",
]
