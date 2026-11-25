#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Migrate display pack manifests to the extended asset-pack v1 contract.

The script walks a root directory (default ``controller/widget_packs``), finds
every ``manifest.json``, and normalizes the pack metadata that the local asset
repository and the published licence gate expect:

* ``type`` and ``schema`` defaults;
* ``license`` (from ``license`` or legacy ``licence``);
* ``provenance``, ``confidence``, ``capabilities``, ``permissions``;
* ``assets`` entries with path/SHA-256/size/MIME/executable/sandbox;
* ``checksums.files`` for every declared or shipped file;
* ``preview`` when a preview image actually exists.

Behaviour is deliberately conservative:

* Dry-run is the default. ``--write`` performs the migration, and ``--backup``
  saves the previous bytes to ``.manifest.json.bak`` before an atomic replace.
* Unknown fields, unknown option values, and existing asset/checksum metadata
  are preserved. The script never imports or executes pack code.
* File discovery never follows symlinks. Absolute paths, drive-qualified paths,
  backslashes, and ``..`` components are rejected.
* Existing hashes are recomputed only for files that exist; declarations for
  missing files are left untouched so the licence gate can report them.
* A manifest that already carries the extended lifecycle block is treated as
  migrated and is not rewritten. This keeps curated first-party manifests
  byte-identical. Pass ``--force`` to re-normalize such a manifest anyway.
* Output is deterministic: sorted keys, two-space indent, UTF-8, trailing
  newline. A second ``--write`` run therefore produces byte-identical bytes.

Usage::

    python scripts/migrate_display_packs.py --root controller/widget_packs
    python scripts/migrate_display_packs.py --root controller/widget_packs \
        --write --backup

The JSON report on stdout has the shape
``{"checked": int, "changed": int, "skipped": int, "errors": [str, ...]}``.
``changed`` counts packs that were written, or would be written in dry-run
mode. The process exits non-zero when any manifest fails.

See ``docs/display-platform/release-notes.md`` and
``docs/display-platform/publishing.md`` for the manifest contract.
"""

from __future__ import annotations

import argparse
import copy
import hashlib
import json
import mimetypes
import os
import re
import stat
import sys
import tempfile
from collections.abc import Iterable, Mapping
from pathlib import Path, PurePosixPath
from typing import Any

ASSET_SCHEMA = "org.ozma.asset-pack/1"
DEFAULT_TYPE = "widget_pack"
MANIFEST_NAME = "manifest.json"
BACKUP_NAME = ".manifest.json.bak"

PACK_TYPES = frozenset({
    "widget_pack",
    "theme_pack",
    "description_pack",
    "import_pack",
})

TYPE_ALIASES = {
    "widget": "widget_pack",
    "widgets": "widget_pack",
    "theme": "theme_pack",
    "themes": "theme_pack",
    "description": "description_pack",
    "import": "import_pack",
}

#: Fields that make an extended manifest self-describing. If all are present,
#: the manifest has already been migrated by an author or a previous run.
MIGRATED_KEYS = (
    "license",
    "provenance",
    "confidence",
    "capabilities",
    "permissions",
    "assets",
    "checksums",
)

PREVIEW_CANDIDATES = (
    "preview.png",
    "preview.jpg",
    "preview.jpeg",
    "preview.webp",
    "preview.svg",
    "preview.gif",
)

EXECUTABLE_SUFFIXES = frozenset({
    ".cjs",
    ".html",
    ".htm",
    ".js",
    ".mjs",
    ".py",
    ".sh",
    ".bash",
    ".wasm",
})

SANDBOX_BY_SUFFIX = {
    ".wasm": "wasm",
}

_ID_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")
_DRIVE_RE = re.compile(r"^[A-Za-z]:")

_EXTRA_MIME_TYPES = {
    ".css": "text/css",
    ".json": "application/json",
    ".md": "text/markdown",
    ".mjs": "text/javascript",
    ".svg": "image/svg+xml",
    ".ttf": "font/ttf",
    ".woff": "font/woff",
    ".woff2": "font/woff2",
}


class MigrationError(Exception):
    """A manifest cannot be migrated without losing data or safety."""


# ── Paths and hashing ────────────────────────────────────────────────────────


def _safe_relative(raw: str) -> str:
    """Return a normalized POSIX relative path or raise MigrationError."""
    text = str(raw or "").strip()
    if not text:
        raise MigrationError("empty asset path")
    if text.startswith("/") or text.startswith("~") or _DRIVE_RE.match(text):
        raise MigrationError(f"unsafe absolute asset path {raw!r}")
    if "\\" in text:
        raise MigrationError(f"backslashes are not allowed in asset paths: {raw!r}")
    parts = PurePosixPath(text).parts
    if not parts or any(part in ("", ".", "..") for part in parts):
        raise MigrationError(f"unsafe asset path {raw!r}")
    return "/".join(parts)


def _resolve_asset(base: Path, relative: str) -> Path:
    """Resolve a safe relative path without traversing any symlink."""
    candidate = base
    for part in relative.split("/"):
        candidate = candidate / part
        if candidate.is_symlink():
            raise MigrationError(f"symlink in asset path: {relative!r}")
    return candidate


def _sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
    descriptor = os.open(path, flags)
    with os.fdopen(descriptor, "rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _mime_for(path: Path, existing: Any = "") -> str:
    if isinstance(existing, str) and existing.strip():
        return existing.strip()
    suffix = path.suffix.lower()
    if suffix in _EXTRA_MIME_TYPES:
        return _EXTRA_MIME_TYPES[suffix]
    guessed, _encoding = mimetypes.guess_type(path.name)
    return guessed or "application/octet-stream"


def _is_executable(path: Path, declared: Any) -> bool:
    if bool(declared):
        return True
    if path.suffix.lower() in EXECUTABLE_SUFFIXES:
        return True
    try:
        return bool(path.stat().st_mode & (stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH))
    except OSError:
        return False


def _sandbox_for(path: Path, executable: bool, declared: Any) -> str | None:
    if isinstance(declared, str) and declared.strip():
        return declared.strip()
    if not executable:
        return None
    return SANDBOX_BY_SUFFIX.get(path.suffix.lower(), "iframe")


# ── Manifest defaults ────────────────────────────────────────────────────────


def _author(manifest: Mapping[str, Any]) -> str:
    author = manifest.get("author")
    if isinstance(author, str) and author.strip():
        return author.strip()
    provenance = manifest.get("provenance")
    if isinstance(provenance, Mapping):
        for key in ("author", "source"):
            value = provenance.get(key)
            if isinstance(value, str) and value.strip():
                return value.strip()
    return ""


def _default_provenance(manifest: Mapping[str, Any], pack_id: str) -> dict[str, Any]:
    author = _author(manifest)
    first_party = author.casefold() == "ozmalabs"
    return {
        "type": "original" if first_party else "community",
        "source": author or pack_id,
        "author": author,
        "method": "authored" if first_party else "unknown",
        "notes": (
            "Migrated by scripts/migrate_display_packs.py; "
            "review provenance before publishing."
        ),
    }


def _default_confidence(manifest: Mapping[str, Any]) -> str:
    provenance = manifest.get("provenance")
    provenance_type = ""
    if isinstance(provenance, Mapping):
        provenance_type = str(provenance.get("type") or "").strip()
    if not provenance_type:
        provenance_type = _default_provenance(manifest, "")["type"]
    return "exact" if provenance_type == "original" else "template"


def _is_migrated(manifest: Mapping[str, Any]) -> bool:
    """Return True when the extended lifecycle block is already present."""
    if any(key not in manifest for key in MIGRATED_KEYS):
        return False
    if not isinstance(manifest.get("provenance"), Mapping):
        return False
    if not isinstance(manifest.get("assets"), list):
        return False
    checksums = manifest.get("checksums")
    if not isinstance(checksums, Mapping):
        return False
    if not isinstance(checksums.get("files", {}), Mapping):
        return False
    if not isinstance(manifest.get("capabilities"), list):
        return False
    if not isinstance(manifest.get("permissions"), list):
        return False
    return bool(str(manifest.get("license") or "").strip())


def _normalize_type(manifest: Mapping[str, Any], *, force: bool) -> str:
    value = str(manifest.get("type") or "").strip()
    if not value:
        return DEFAULT_TYPE
    value = TYPE_ALIASES.get(value, value)
    if value in PACK_TYPES:
        return value
    if force:
        return DEFAULT_TYPE
    raise MigrationError(f"unsupported pack type {value!r}")


def _forced_base(path: Path) -> dict[str, Any]:
    """Minimal, deterministic starting point for invalid JSON under --force."""
    pack_id = path.parent.name
    return {
        "id": pack_id if _ID_RE.match(pack_id) else "pack",
        "name": pack_id or "pack",
        "version": "1.0.0",
    }


# ── Assets and checksums ─────────────────────────────────────────────────────


def _normalize_asset(
    base: Path,
    raw: Mapping[str, Any],
    *,
    label: str,
) -> dict[str, Any]:
    path_value = raw.get("path")
    if not isinstance(path_value, str) or not path_value.strip():
        raise MigrationError(f"{label}: asset entry missing path")
    relative = _safe_relative(path_value)
    target = _resolve_asset(base, relative)

    extra = {
        key: copy.deepcopy(value)
        for key, value in raw.items()
        if key
        not in {
            "path",
            "sha256",
            "size",
            "size_bytes",
            "mime",
            "mime_type",
            "executable",
            "sandbox",
        }
    }

    existing_sha = raw.get("sha256", raw.get("hash", ""))
    if isinstance(existing_sha, Mapping):
        existing_sha = existing_sha.get("sha256", existing_sha.get("hash", ""))
    existing_sha = str(existing_sha or "").strip().lower()
    existing_size = raw.get("size", raw.get("size_bytes", 0))
    try:
        size = int(existing_size or 0)
    except (TypeError, ValueError):
        size = 0
    existing_mime = raw.get("mime", raw.get("mime_type", ""))
    existing_executable = bool(raw.get("executable", False))
    existing_sandbox = raw.get("sandbox")

    if target.exists():
        if not target.is_file():
            raise MigrationError(f"{label}: asset is not a regular file: {relative!r}")
        digest = _sha256_file(target)
        size = target.stat().st_size
        mime = _mime_for(target, existing_mime)
        executable = _is_executable(target, existing_executable)
        sandbox = _sandbox_for(target, executable, existing_sandbox)
    else:
        # Keep old declarations for missing files: the licence gate reports
        # them, and silently dropping metadata would lose information.
        digest = existing_sha
        mime = _mime_for(target, existing_mime)
        executable = _is_executable(target, existing_executable)
        sandbox = _sandbox_for(target, executable, existing_sandbox)

    asset = dict(extra)
    asset.update({
        "path": relative,
        "sha256": digest,
        "size": size,
        "mime": mime,
        "executable": executable,
        "sandbox": sandbox,
    })
    return asset


def _discover_files(base: Path, manifest_path: Path) -> tuple[list[str], list[str]]:
    """Return relative payload paths and symlink errors, never following links."""
    found: list[str] = []
    errors: list[str] = []
    for directory, dirnames, filenames in os.walk(base, followlinks=False):
        current = Path(directory)
        # os.walk does not descend into symlinked directories when
        # followlinks=False, but prune them explicitly for clarity.
        dirnames[:] = sorted(
            name for name in dirnames if not (current / name).is_symlink()
        )
        for name in sorted(filenames):
            candidate = current / name
            if candidate == manifest_path or name == BACKUP_NAME:
                continue
            relative = candidate.relative_to(base).as_posix()
            if candidate.is_symlink():
                errors.append(f"symlink payload is not supported: {relative!r}")
                continue
            if not candidate.is_file():
                continue
            found.append(relative)
    return sorted(found), errors


def _declared_assets(
    manifest: Mapping[str, Any], label: str
) -> tuple[dict[str, dict[str, Any]], list[str]]:
    raw_assets = manifest.get("assets", [])
    if raw_assets in (None, ""):
        raw_assets = []
    if not isinstance(raw_assets, list):
        raise MigrationError(f"{label}: manifest assets must be a list")

    declared: dict[str, dict[str, Any]] = {}
    errors: list[str] = []
    for item in raw_assets:
        if not isinstance(item, Mapping):
            errors.append(f"{label}: asset entry is not an object")
            continue
        raw_path = item.get("path")
        try:
            relative = _safe_relative(str(raw_path or ""))
        except MigrationError as exc:
            errors.append(f"{label}: {exc}")
            continue
        current = declared.get(relative)
        if current is None:
            declared[relative] = copy.deepcopy(dict(item))
            continue
        # Preserve fields from duplicate declarations rather than dropping
        # them; the last non-empty value wins for scalar standard fields.
        merged = copy.deepcopy(current)
        for key, value in item.items():
            if value not in (None, "", [], {}):
                merged[key] = copy.deepcopy(value)
        declared[relative] = merged
    return declared, errors


def _checksum_files(manifest: Mapping[str, Any]) -> dict[str, Any]:
    checksums = manifest.get("checksums")
    if not isinstance(checksums, Mapping):
        return {}
    files = checksums.get("files", checksums)
    collected: dict[str, Any] = {}

    def add(raw_path: Any, digest: Any) -> None:
        try:
            relative = _safe_relative(str(raw_path or ""))
        except MigrationError as exc:
            raise MigrationError(f"checksums.files: {exc}") from exc
        collected[relative] = copy.deepcopy(digest)

    if isinstance(files, Mapping):
        for key, value in files.items():
            if key in {"algorithm", "schema"}:
                continue
            add(key, value)
        return collected
    if isinstance(files, list):
        for item in files:
            if not isinstance(item, Mapping):
                continue
            add(item.get("path"), item.get("sha256", item.get("hash", "")))
    return collected


def _normalize_assets(
    manifest: Mapping[str, Any],
    base: Path,
    manifest_path: Path,
    label: str,
) -> tuple[list[dict[str, Any]], list[str], list[str]]:
    declared, errors = _declared_assets(manifest, label)
    found, symlink_errors = _discover_files(base, manifest_path)
    errors.extend(symlink_errors)

    for relative in found:
        declared.setdefault(relative, {"path": relative})

    assets: list[dict[str, Any]] = []
    for relative in sorted(declared):
        try:
            assets.append(
                _normalize_asset(base, declared[relative], label=label)
            )
        except MigrationError as exc:
            errors.append(f"{label}: {exc}")
    return assets, errors, found


def _normalize_checksums(
    manifest: Mapping[str, Any], assets: list[dict[str, Any]]
) -> dict[str, Any]:
    raw = manifest.get("checksums")
    checksums: dict[str, Any] = (
        copy.deepcopy(dict(raw)) if isinstance(raw, Mapping) else {}
    )
    files = _checksum_files(manifest)
    for asset in assets:
        digest = str(asset.get("sha256") or "").strip()
        if digest:
            files[asset["path"]] = digest
    checksums["algorithm"] = "sha256"
    checksums["files"] = files
    return checksums


# ── Normalization ────────────────────────────────────────────────────────────


def _detect_preview(base: Path, manifest: Mapping[str, Any]) -> str:
    existing = manifest.get("preview")
    if isinstance(existing, str) and existing.strip():
        try:
            relative = _safe_relative(existing)
            target = _resolve_asset(base, relative)
        except MigrationError as exc:
            raise MigrationError(f"preview: {exc}") from exc
        if target.is_file():
            return relative
        # The declaration is stale. Fall through and look for a real preview
        # file rather than publishing a broken reference.
    for name in PREVIEW_CANDIDATES:
        candidate = base / name
        if candidate.is_file() and not candidate.is_symlink():
            return name
    return ""


def _normalize_manifest(
    manifest: Mapping[str, Any],
    *,
    base: Path,
    manifest_path: Path,
    label: str,
    force: bool,
) -> tuple[dict[str, Any], list[str]]:
    data = copy.deepcopy(dict(manifest))
    errors: list[str] = []

    data["type"] = _normalize_type(manifest, force=force)
    data["schema"] = ASSET_SCHEMA
    data["license"] = str(
        manifest.get("license", manifest.get("licence", "")) or ""
    ).strip()

    provenance = manifest.get("provenance")
    if isinstance(provenance, Mapping) and provenance:
        data["provenance"] = copy.deepcopy(dict(provenance))
    else:
        data["provenance"] = _default_provenance(manifest, str(data.get("id") or ""))

    confidence = str(manifest.get("confidence") or "").strip()
    data["confidence"] = confidence or _default_confidence(manifest)

    for key, default in (("capabilities", ["rendered_widget"]), ("permissions", [])):
        value = manifest.get(key)
        if value is None:
            data[key] = copy.deepcopy(default)
        elif isinstance(value, list):
            data[key] = copy.deepcopy(value)
        else:
            errors.append(f"{label}: {key} must be a list")

    preview = _detect_preview(base, manifest)
    if preview:
        data["preview"] = preview
    else:
        # Never publish a preview reference that does not resolve to a file.
        data.pop("preview", None)

    assets, asset_errors, _found = _normalize_assets(
        manifest, base, manifest_path, label
    )
    errors.extend(asset_errors)
    data["assets"] = assets
    data["checksums"] = _normalize_checksums(manifest, assets)
    return data, errors


def _serialize(data: Mapping[str, Any]) -> bytes:
    text = json.dumps(
        data,
        indent=2,
        sort_keys=True,
        ensure_ascii=False,
    )
    return (text + "\n").encode("utf-8")


# ── Writing ──────────────────────────────────────────────────────────────────


def _atomic_write(path: Path, payload: bytes) -> None:
    descriptor, temporary = tempfile.mkstemp(
        prefix=".manifest.", suffix=".tmp", dir=path.parent
    )
    temporary_path = Path(temporary)
    try:
        with os.fdopen(descriptor, "wb") as handle:
            handle.write(payload)
            handle.flush()
            os.fsync(handle.fileno())
        os.chmod(temporary_path, 0o644)
        os.replace(temporary_path, path)
    except BaseException:
        temporary_path.unlink(missing_ok=True)
        raise


def _read_manifest(path: Path) -> tuple[bytes, Mapping[str, Any], os.stat_result]:
    try:
        before = path.stat()
        original = path.read_bytes()
        after = path.stat()
    except OSError as exc:
        raise MigrationError(f"cannot read manifest: {exc}") from exc
    if (before.st_mtime_ns, before.st_size) != (
        after.st_mtime_ns,
        after.st_size,
    ):
        raise MigrationError("manifest changed while reading; re-run after review")
    try:
        manifest = json.loads(original.decode("utf-8"))
    except (UnicodeDecodeError, ValueError) as exc:
        raise MigrationError(f"invalid JSON: {exc}") from exc
    if not isinstance(manifest, Mapping):
        raise MigrationError("manifest must be a JSON object")
    return original, manifest, before


def _check_schema(manifest: Mapping[str, Any], *, force: bool) -> None:
    schema = str(manifest.get("schema") or "").strip()
    if schema and schema != ASSET_SCHEMA and not force:
        raise MigrationError(
            f"manifest schema {schema!r} is newer or unknown; use --force to "
            f"overwrite with {ASSET_SCHEMA}"
        )


def _migrate_one(
    manifest_path: Path,
    *,
    write: bool,
    backup: bool,
    force: bool,
) -> tuple[bool, str | None]:
    """Return ``(changed, error)`` for one manifest."""
    label = manifest_path.as_posix()
    try:
        try:
            original, manifest, before = _read_manifest(manifest_path)
        except MigrationError:
            # --force is the documented escape hatch for an unreadable or
            # invalid manifest. Without it, fail closed and leave the bytes.
            if not force or not manifest_path.is_file():
                raise
            original = manifest_path.read_bytes()
            before = manifest_path.stat()
            manifest = _forced_base(manifest_path)
        _check_schema(manifest, force=force)
        if _is_migrated(manifest) and not force:
            return False, None

        data, errors = _normalize_manifest(
            manifest,
            base=manifest_path.parent,
            manifest_path=manifest_path,
            label=label,
            force=force,
        )
        if errors:
            return False, "; ".join(errors)

        payload = _serialize(data)
        if payload == original:
            return False, None
        if not write:
            return True, None

        try:
            current = manifest_path.stat()
        except OSError as exc:
            return False, f"cannot re-stat manifest: {exc}"
        if (current.st_mtime_ns, current.st_size) != (
            before.st_mtime_ns,
            before.st_size,
        ):
            return False, "manifest changed during migration; re-run after review"

        if backup:
            _atomic_write(manifest_path.parent / BACKUP_NAME, original)
        _atomic_write(manifest_path, payload)
        return True, None
    except MigrationError as exc:
        return False, str(exc)
    except OSError as exc:
        return False, f"cannot migrate manifest: {exc}"


def _discover_manifests(root: Path) -> tuple[list[Path], list[str]]:
    manifests: list[Path] = []
    errors: list[str] = []
    if not root.is_dir():
        return [], [f"{root.as_posix()}: root is not a directory"]
    for directory, dirnames, filenames in os.walk(root, followlinks=False):
        current = Path(directory)
        dirnames[:] = sorted(
            name for name in dirnames if not (current / name).is_symlink()
        )
        for name in sorted(filenames):
            if name != MANIFEST_NAME:
                continue
            candidate = current / name
            if candidate.is_symlink():
                errors.append(
                    f"{candidate.as_posix()}: manifest symlink is not supported"
                )
                continue
            manifests.append(candidate)
    manifests.sort(key=lambda item: item.as_posix())
    return manifests, errors


def migrate_root(
    root: Path | str,
    *,
    write: bool = False,
    backup: bool = False,
    force: bool = False,
) -> dict[str, Any]:
    """Migrate every manifest under ``root`` and return the JSON report."""
    root_path = Path(root)
    manifests, errors = _discover_manifests(root_path)
    changed = 0
    skipped = 0
    for manifest_path in manifests:
        was_changed, error = _migrate_one(
            manifest_path,
            write=write,
            backup=backup,
            force=force,
        )
        if error is not None:
            errors.append(f"{manifest_path.as_posix()}: {error}")
        elif was_changed:
            changed += 1
        else:
            skipped += 1
    report: dict[str, Any] = {
        "checked": len(manifests),
        "changed": changed,
        "skipped": skipped,
        "errors": sorted(errors),
    }
    return report


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=(
            "Normalize display pack manifests to org.ozma.asset-pack/1. "
            "Dry-run is the default; pass --write to modify files."
        )
    )
    parser.add_argument(
        "--root",
        default="controller/widget_packs",
        help="directory to scan recursively (default: controller/widget_packs)",
    )
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument(
        "--dry-run",
        action="store_true",
        help="report changes without writing (default)",
    )
    mode.add_argument(
        "--write",
        action="store_true",
        help="write normalized manifests atomically",
    )
    parser.add_argument(
        "--backup",
        action="store_true",
        help=f"save the previous bytes to {BACKUP_NAME} before writing",
    )
    parser.add_argument(
        "--force",
        action="store_true",
        help=(
            "overwrite a newer/invalid manifest or re-normalize an already "
            "migrated one"
        ),
    )
    return parser


def main(argv: Iterable[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(list(argv) if argv is not None else None)
    if args.backup and not args.write:
        parser.error("--backup requires --write")

    report = migrate_root(
        args.root,
        write=args.write,
        backup=args.backup,
        force=args.force,
    )
    print(json.dumps(report, indent=2, sort_keys=True))
    return 0 if not report["errors"] else 1


if __name__ == "__main__":
    sys.exit(main())
