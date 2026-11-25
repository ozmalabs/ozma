# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Private, local storage for imported display sources.

Imports are private by default and are never published automatically. The
store keeps the raw source byte-for-byte, every retained asset, the semantic
widget projection, and the lossless capture report. Nothing here executes
source code or contacts the network.
"""

from __future__ import annotations

import json
import os
import re
import shutil
import tempfile
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

try:  # controller modules are imported top-level (controller/ is on sys.path)
    from display_importers.base import ImportError as DisplayImportError
    from display_importers.base import ImportResult, sha256_hex
    from display_importers.registry import import_source as run_import
except ImportError:  # package-style import used by some tooling
    from .display_importers.base import ImportError as DisplayImportError
    from .display_importers.base import ImportResult, sha256_hex
    from .display_importers.registry import import_source as run_import

IMPORT_PACK_SCHEMA = "org.ozma.import-pack/1"
DEFAULT_MAX_IMPORT_BYTES = 32 * 1024 * 1024
_ID_RE = re.compile(r"^imp-[0-9a-f]{24}$")


class ImportStoreError(ValueError):
    """Base error for private import storage."""

    status_code = 400
    code = "import_error"


class ImportTooLargeError(ImportStoreError):
    """The source exceeds the configured import budget."""

    status_code = 413
    code = "import_too_large"


class ImportNotFoundError(ImportStoreError):
    """Unknown import id."""

    status_code = 404
    code = "import_not_found"


class ImportConflictError(ImportStoreError):
    """Stored bytes do not match the manifest."""

    status_code = 409
    code = "import_conflict"


def _utc_now() -> str:
    return datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")


def _canonical_json(value: Any) -> str:
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False)


def _safe_component(name: str, fallback: str) -> str:
    """Keep a display name safe for use as a single path component."""
    candidate = Path(str(name or "")).name
    candidate = re.sub(r"[^A-Za-z0-9._-]+", "_", candidate).strip("._")
    if not candidate or candidate in {".", ".."}:
        return fallback
    return candidate[:200]


class ImportStore:
    """Content-addressed private import packs on the local filesystem."""

    def __init__(
        self,
        root: str | Path | None = None,
        *,
        max_import_bytes: int = DEFAULT_MAX_IMPORT_BYTES,
    ) -> None:
        if max_import_bytes <= 0:
            raise ValueError("max_import_bytes must be positive")
        self.root = Path(root) if root is not None else Path(__file__).parent / "asset_cache" / "imports"
        self.max_import_bytes = int(max_import_bytes)

    # ── paths ─────────────────────────────────────────────────────────────

    def pack_dir(self, import_id: str) -> Path:
        if not _ID_RE.match(str(import_id or "")):
            raise ImportStoreError(f"invalid import id: {import_id!r}")
        return self.root / import_id

    def ensure(self) -> None:
        self.root.mkdir(parents=True, exist_ok=True)

    # ── write ─────────────────────────────────────────────────────────────

    def save(
        self,
        name: str,
        data: bytes,
        *,
        format: str | None = None,
    ) -> dict[str, Any]:
        """Import and persist a private pack; idempotent for identical input."""
        raw = bytes(data)
        if len(raw) > self.max_import_bytes:
            raise ImportTooLargeError(
                f"source is {len(raw)} bytes; limit is {self.max_import_bytes}"
            )
        result = run_import(name, raw, format=format)
        return self.save_result(result, name=name)

    def save_result(self, result: ImportResult, *, name: str = "") -> dict[str, Any]:
        """Persist an already-imported result."""
        result.validate()
        raw = bytes(result.raw)
        if len(raw) > self.max_import_bytes:
            raise ImportTooLargeError(
                f"source is {len(raw)} bytes; limit is {self.max_import_bytes}"
            )

        import_id = "imp-" + sha256_hex(
            f"{name}\0{result.format}\0".encode("utf-8") + raw
        )[:24]
        target = self.pack_dir(import_id)
        if (target / "manifest.json").is_file():
            return self.get(import_id)

        self.ensure()
        staging = Path(tempfile.mkdtemp(prefix=f".{import_id}-", dir=str(self.root)))
        try:
            source_name = _safe_component(name, f"source{Path(name).suffix or '.bin'}")
            raw_path = staging / "source" / source_name
            raw_path.parent.mkdir(parents=True, exist_ok=True)
            raw_path.write_bytes(raw)

            asset_entries: list[dict[str, Any]] = []
            asset_dir = staging / "assets"
            asset_dir.mkdir(parents=True, exist_ok=True)
            for asset in result.assets:
                suffix = Path(asset.path).suffix[:16]
                filename = f"{asset.sha256}{suffix}"
                path = asset_dir / filename
                if not path.is_file():
                    path.write_bytes(bytes(asset.data))
                entry = asset.to_asset_ref()
                entry["stored_path"] = f"assets/{filename}"
                asset_entries.append(entry)

            manifest: dict[str, Any] = {
                "schema": IMPORT_PACK_SCHEMA,
                "id": import_id,
                "name": str(name or source_name),
                "format": result.format,
                "version": result.version,
                "private": True,
                "published": False,
                "created_at": _utc_now(),
                "source": dict(result.source_document),
                "source_capture": dict(result.source_capture),
                "widgets": [dict(widget) for widget in result.widgets],
                "assets": asset_entries,
                "unknown": dict(result.unknown),
                "warnings": list(result.warnings),
                "extensions": dict(result.extensions),
                "raw": {
                    "path": f"source/{source_name}",
                    "sha256": sha256_hex(raw),
                    "size": len(raw),
                },
            }
            (staging / "manifest.json").write_text(
                json.dumps(manifest, indent=2, sort_keys=True, ensure_ascii=False) + "\n",
                encoding="utf-8",
            )
            if target.exists():
                raise ImportConflictError(f"import already exists: {import_id}")
            os.replace(staging, target)
            return manifest
        except Exception:
            shutil.rmtree(staging, ignore_errors=True)
            raise

    # ── read ──────────────────────────────────────────────────────────────

    def get(self, import_id: str) -> dict[str, Any]:
        target = self.pack_dir(import_id)
        manifest_path = target / "manifest.json"
        if not manifest_path.is_file():
            raise ImportNotFoundError(f"unknown import: {import_id}")
        data = json.loads(manifest_path.read_text(encoding="utf-8"))
        if data.get("id") != import_id:
            raise ImportConflictError("manifest id does not match its directory")
        return data

    def list_imports(self) -> list[dict[str, Any]]:
        if not self.root.is_dir():
            return []
        items: list[dict[str, Any]] = []
        for entry in self.root.iterdir():
            if not entry.is_dir() or not _ID_RE.match(entry.name):
                continue
            try:
                items.append(self.get(entry.name))
            except ImportStoreError:
                continue
        return sorted(items, key=lambda item: (item.get("created_at", ""), item.get("id", "")))

    def load_raw(self, import_id: str) -> bytes:
        manifest = self.get(import_id)
        raw_info = manifest.get("raw") or {}
        relative = str(raw_info.get("path") or "")
        if not relative or relative.startswith("/") or ".." in Path(relative).parts:
            raise ImportConflictError("manifest has an unsafe raw path")
        data = (self.pack_dir(import_id) / relative).read_bytes()
        if sha256_hex(data) != raw_info.get("sha256"):
            raise ImportConflictError("raw source hash mismatch")
        return data

    def load_asset(self, import_id: str, sha256: str) -> bytes:
        manifest = self.get(import_id)
        for asset in manifest.get("assets") or []:
            if asset.get("sha256") == sha256:
                relative = str(asset.get("stored_path") or "")
                if not relative.startswith("assets/") or ".." in Path(relative).parts:
                    raise ImportConflictError("manifest has an unsafe asset path")
                data = (self.pack_dir(import_id) / relative).read_bytes()
                if sha256_hex(data) != sha256:
                    raise ImportConflictError("asset hash mismatch")
                return data
        raise ImportNotFoundError(f"unknown asset: {sha256}")

    def delete(self, import_id: str) -> bool:
        target = self.pack_dir(import_id)
        if not (target / "manifest.json").is_file():
            raise ImportNotFoundError(f"unknown import: {import_id}")
        shutil.rmtree(target)
        return True


__all__ = [
    "DEFAULT_MAX_IMPORT_BYTES",
    "IMPORT_PACK_SCHEMA",
    "ImportConflictError",
    "ImportNotFoundError",
    "ImportStore",
    "ImportStoreError",
    "ImportTooLargeError",
    "DisplayImportError",
]
