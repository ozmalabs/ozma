#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Licence gate for display widget, theme, and Connect asset manifests.

The gate is intentionally read-only: it parses JSON manifests, hashes declared
assets, and reports a deterministic result.  It never imports or executes pack
code and never touches the network.

Usage::

    python scripts/check_display_licenses.py --root . --mode local
    python scripts/check_display_licenses.py --root . --mode published --json
    python scripts/check_display_licenses.py \
        --root . --mode local path/to/manifest.json

Published mode is the community gate: only the SPDX licences in
:data:`ALLOWED_LICENSES` pass.  Local mode additionally permits AGPL-family
licences for the configured first-party author, which keeps the built-in
``retro-gauges`` pack installable without opening the public allowlist.

See ``docs/display-platform/publishing.md`` and
``docs/display-platform-execution-plan.md`` §11 for policy.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import sys
from collections.abc import Iterable, Mapping, Sequence
from pathlib import Path, PurePosixPath
from typing import Any

ALLOWED_LICENSES = frozenset({
    "MIT",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "ISC",
    "Apache-2.0",
    "OFL-1.1",
    "CC0-1.0",
})

LICENSE_ALIASES = {
    "apache 2.0": "Apache-2.0",
    "apache-2": "Apache-2.0",
    "bsd": "BSD-3-Clause",
    "cc0": "CC0-1.0",
    "mit license": "MIT",
    "ofl": "OFL-1.1",
}

DEFAULT_FIRST_PARTY_AUTHOR = "ozmalabs"

MANIFEST_PATTERNS = (
    ("controller", "widget_packs", "*", "manifest.json"),
    ("web", "display", "themes", "manifest.json"),
)

_OPERATOR_OR = re.compile(r"\s+OR\s+", re.IGNORECASE)
_OPERATOR_AND = re.compile(r"\s+AND\s+", re.IGNORECASE)
_DRIVE_PATH = re.compile(r"^[A-Za-z]:")


# ── Manifest discovery ───────────────────────────────────────────────────────


def _display_path(path: Path, root: Path) -> str:
    """Return a stable, root-relative POSIX path for reports."""
    try:
        relative = os.path.relpath(str(path), str(root))
    except ValueError:  # different drives on Windows
        return path.as_posix()
    if relative == "." or relative.startswith(".." + os.sep):
        return path.as_posix()
    return Path(relative).as_posix()


def discover_manifests(
    root: Path,
    explicit: Sequence[str] = (),
) -> tuple[list[Path], list[str]]:
    """Find default manifests plus any explicit files.

    Returns ``(manifests, errors)``.  Missing explicit files are errors;
    missing default directories are simply skipped so the gate runs on a
    partial checkout or a Connect-only tree.
    """
    found: list[Path] = []
    errors: list[str] = []

    if root.is_dir():
        for parts in MANIFEST_PATTERNS:
            found.extend(root.glob("/".join(parts)))
        connect_assets = root / "connect" / "assets"
        if connect_assets.is_dir():
            found.extend(connect_assets.rglob("manifest.json"))

    for raw in explicit:
        candidate = Path(raw)
        if not candidate.is_file():
            errors.append(f"{raw}: explicit manifest does not exist")
            continue
        found.append(candidate)

    deduped: dict[str, Path] = {}
    for path in found:
        if not path.is_file():
            continue
        key = str(path.resolve())
        deduped.setdefault(key, path)

    manifests = sorted(
        deduped.values(),
        key=lambda item: (_display_path(item, root), str(item.resolve())),
    )
    return manifests, errors


# ── Licence evaluation ───────────────────────────────────────────────────────


def _normalize_license(expression: str) -> str:
    text = (expression or "").strip()
    return LICENSE_ALIASES.get(text.lower(), text)


def _is_agpl(atom: str) -> bool:
    return "AGPL" in atom.upper()


def _atom_allowed(atom: str, *, mode: str, first_party: bool) -> bool:
    if atom in ALLOWED_LICENSES:
        return True
    return mode == "local" and first_party and _is_agpl(atom)


def _expression_allowed(
    expression: str, *, mode: str, first_party: bool
) -> bool:
    """Evaluate simple SPDX ``AND``/``OR`` expressions conservatively.

    ``AND`` needs every branch allowed; ``OR`` needs at least one.  Parentheses
    are treated as grouping only.  This mirrors the repository allowlist.
    """
    cleaned = expression.replace("(", " ").replace(")", " ").strip()
    if not cleaned:
        return False
    or_parts = _OPERATOR_OR.split(cleaned)
    if len(or_parts) > 1:
        return any(
            _expression_allowed(part, mode=mode, first_party=first_party)
            for part in or_parts
        )
    and_parts = _OPERATOR_AND.split(cleaned)
    if len(and_parts) > 1:
        return all(
            _expression_allowed(part, mode=mode, first_party=first_party)
            for part in and_parts
        )
    return _atom_allowed(cleaned, mode=mode, first_party=first_party)


def _license_error(expression: str, *, mode: str, first_party: bool) -> str:
    if not expression:
        return "missing licence"
    upper = expression.upper()
    if _is_agpl(expression):
        if mode == "local":
            return (
                "AGPL licences are only allowed for the configured "
                "first-party author"
            )
        return "AGPL licences are not allowed for published packs"
    if "GPL" in upper:
        return "GPL licences are not allowed"
    if (
        "CC-BY-NC" in upper
        or "NONCOMMERCIAL" in upper
        or "NON-COMMERCIAL" in upper
    ):
        return "non-commercial licences are not allowed"
    if "PERSONAL USE" in upper or "PERSONAL-USE" in upper:
        return "personal-use licences are not allowed"
    return f"licence {expression!r} is not on the allowlist"


def _author_candidates(manifest: Mapping[str, Any]) -> set[str]:
    candidates: set[str] = set()
    direct = manifest.get("author")
    if isinstance(direct, str) and direct.strip():
        candidates.add(direct.strip().casefold())
    provenance = manifest.get("provenance")
    if isinstance(provenance, Mapping):
        for key in ("author", "source"):
            value = provenance.get(key)
            if isinstance(value, str) and value.strip():
                candidates.add(value.strip().casefold())
    return candidates


def _is_first_party(
    manifest: Mapping[str, Any], first_party_author: str
) -> bool:
    expected = (first_party_author or "").strip().casefold()
    if not expected:
        return False
    return expected in _author_candidates(manifest)


# ── Asset declarations and hashes ────────────────────────────────────────────


def _safe_asset_path(raw: str) -> tuple[str | None, str | None]:
    text = (raw or "").replace("\\", "/").strip()
    if not text:
        return None, "empty asset path"
    if text.startswith("/") or text.startswith("~") or _DRIVE_PATH.match(text):
        return None, f"unsafe asset path {raw!r}"
    parts = PurePosixPath(text).parts
    if any(part in ("", ".", "..") for part in parts):
        return None, f"unsafe asset path {raw!r}"
    return "/".join(parts), None


def _digest_value(value: Any) -> str:
    if isinstance(value, Mapping):
        value = value.get("sha256", value.get("hash", ""))
    return str(value or "").strip().lower()


def _declared_assets(
    manifest: Mapping[str, Any],
) -> tuple[dict[str, dict[str, Any]], list[str]]:
    declared: dict[str, dict[str, Any]] = {}
    issues: list[str] = []

    def add(
        raw_path: Any,
        digest: Any,
        *,
        executable: bool = False,
        sandbox: Any = None,
    ) -> None:
        path, problem = _safe_asset_path(str(raw_path or ""))
        if problem is not None:
            issues.append(problem)
            return
        assert path is not None
        entry = declared.setdefault(
            path,
            {"sha256": "", "executable": False, "sandbox": None},
        )
        normalized = _digest_value(digest)
        if normalized:
            if entry["sha256"] and entry["sha256"] != normalized:
                issues.append(f"conflicting SHA-256 values for {path!r}")
            entry["sha256"] = normalized
        if executable:
            entry["executable"] = True
        if sandbox not in (None, ""):
            sandbox_text = str(sandbox).strip()
            if sandbox_text.lower() not in {"none", "null", "false", "0"}:
                entry["sandbox"] = sandbox_text

    raw_assets = manifest.get("assets", [])
    if isinstance(raw_assets, list):
        for item in raw_assets:
            if not isinstance(item, Mapping):
                issues.append("asset entry is not an object")
                continue
            add(
                item.get("path"),
                item.get("sha256", item.get("hash", "")),
                executable=bool(item.get("executable", False)),
                sandbox=item.get("sandbox"),
            )
    elif raw_assets not in (None, ""):
        issues.append("manifest assets must be a list")

    checksums = manifest.get("checksums")
    if isinstance(checksums, Mapping):
        files = checksums.get("files", checksums)
        if isinstance(files, Mapping):
            for raw_path, digest in files.items():
                if raw_path in {"algorithm", "schema"}:
                    continue
                add(raw_path, digest)
        elif isinstance(files, list):
            for item in files:
                if isinstance(item, Mapping):
                    add(
                        item.get("path"),
                        item.get("sha256", item.get("hash", "")),
                    )

    return declared, issues


def _sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _check_assets(
    manifest_path: Path,
    manifest: Mapping[str, Any],
    *,
    errors: list[str],
    warnings: list[str],
) -> tuple[int, int]:
    """Verify declarations; return ``(asset_count, hashes_verified)``."""
    declared, issues = _declared_assets(manifest)
    errors.extend(issues)
    verified = 0
    base = manifest_path.parent

    for path in sorted(declared):
        entry = declared[path]
        if entry["executable"] and not entry["sandbox"]:
            errors.append(
                f"executable asset {path!r} has no sandbox declaration"
            )
        target = base.joinpath(*path.split("/"))
        if not target.is_file():
            errors.append(f"declared asset missing: {path!r}")
            continue
        expected = entry["sha256"]
        if not expected:
            errors.append(f"declared asset has no SHA-256: {path!r}")
            continue
        actual = _sha256_file(target)
        if actual != expected:
            errors.append(
                f"SHA-256 mismatch for {path!r}: expected {expected}, "
                f"got {actual}"
            )
            continue
        verified += 1

    preview = manifest.get("preview")
    if isinstance(preview, str) and preview.strip():
        preview_path, problem = _safe_asset_path(preview)
        if problem is not None:
            warnings.append(f"preview path is not usable: {problem}")
        elif not base.joinpath(*preview_path.split("/")).is_file():
            warnings.append(f"preview file missing: {preview!r}")

    return len(declared), verified


# ── Gate ─────────────────────────────────────────────────────────────────────


def check_manifest(
    manifest_path: Path,
    *,
    root: Path,
    mode: str,
    first_party_author: str,
) -> dict[str, Any]:
    """Evaluate one manifest and return its report plus check record."""
    label = _display_path(manifest_path, root)
    errors: list[str] = []
    warnings: list[str] = []
    record: dict[str, Any] = {
        "manifest": label,
        "id": "",
        "license": "",
        "author": "",
        "first_party": False,
        "assets": 0,
        "hashes_verified": 0,
    }

    try:
        text = manifest_path.read_text(encoding="utf-8")
    except OSError as exc:
        errors.append(f"{label}: cannot read manifest: {exc}")
        return {"record": record, "errors": errors, "warnings": warnings}

    try:
        manifest = json.loads(text)
    except ValueError as exc:
        errors.append(f"{label}: invalid JSON: {exc}")
        return {"record": record, "errors": errors, "warnings": warnings}

    if not isinstance(manifest, Mapping):
        errors.append(f"{label}: manifest must be a JSON object")
        return {"record": record, "errors": errors, "warnings": warnings}

    license_value = manifest.get("license", manifest.get("licence", ""))
    license_text = str(license_value or "").strip()
    normalized = _normalize_license(license_text)
    author = str(manifest.get("author") or "").strip()
    first_party = _is_first_party(manifest, first_party_author)

    record.update({
        "id": str(manifest.get("id") or ""),
        "license": license_text,
        "author": author,
        "first_party": first_party,
    })

    if not normalized:
        errors.append(_license_error("", mode=mode, first_party=first_party))
    elif not _expression_allowed(
        normalized, mode=mode, first_party=first_party
    ):
        reason = _license_error(
            normalized, mode=mode, first_party=first_party
        )
        errors.append(f"{reason} ({license_text!r})")

    assets, verified = _check_assets(
        manifest_path, manifest, errors=errors, warnings=warnings
    )
    record["assets"] = assets
    record["hashes_verified"] = verified

    prefixed_errors = [f"{label}: {message}" for message in errors]
    prefixed_warnings = [f"{label}: {message}" for message in warnings]
    return {
        "record": record,
        "errors": prefixed_errors,
        "warnings": prefixed_warnings,
    }


def check(
    *,
    root: Path,
    mode: str,
    first_party_author: str = DEFAULT_FIRST_PARTY_AUTHOR,
    explicit: Sequence[str] = (),
) -> dict[str, Any]:
    manifests, discovery_errors = discover_manifests(root, explicit)
    errors = list(discovery_errors)
    warnings: list[str] = []
    checked: list[dict[str, Any]] = []

    for manifest_path in manifests:
        outcome = check_manifest(
            manifest_path,
            root=root,
            mode=mode,
            first_party_author=first_party_author,
        )
        checked.append(outcome["record"])
        errors.extend(outcome["errors"])
        warnings.extend(outcome["warnings"])

    if not manifests and not errors:
        warnings.append("no display manifests found")

    checked.sort(key=lambda item: item["manifest"])
    return {
        "ok": not errors,
        "errors": sorted(errors),
        "warnings": sorted(warnings),
        "checked": checked,
    }


def _print_human(result: Mapping[str, Any], *, mode: str) -> None:
    status = "OK" if result["ok"] else "FAILED"
    print(
        f"display licence gate: {status} "
        f"(mode={mode}, manifests={len(result['checked'])})"
    )
    for warning in result["warnings"]:
        print(f"warning: {warning}")
    for error in result["errors"]:
        print(f"error: {error}")
    if result["ok"]:
        print("all checked manifests satisfy the display licence policy")


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=(
            "Check display widget/theme/asset manifests against the "
            "published licence allowlist and verify declared asset hashes."
        )
    )
    parser.add_argument(
        "--root",
        default=".",
        help="repository root to scan (default: current directory)",
    )
    parser.add_argument(
        "--mode",
        choices=("local", "published"),
        default="local",
        help="local permits first-party AGPL; published uses the allowlist",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="emit deterministic machine-readable JSON",
    )
    parser.add_argument(
        "--first-party-author",
        default=DEFAULT_FIRST_PARTY_AUTHOR,
        help="author treated as first-party in local mode",
    )
    parser.add_argument(
        "paths",
        nargs="*",
        help="additional manifest files to check",
    )
    return parser


def main(argv: Iterable[str] | None = None) -> int:
    args = build_parser().parse_args(list(argv) if argv is not None else None)
    result = check(
        root=Path(args.root),
        mode=args.mode,
        first_party_author=args.first_party_author,
        explicit=args.paths,
    )
    if args.json:
        print(json.dumps(result, indent=2, sort_keys=True))
    else:
        _print_human(result, mode=args.mode)
    return 0 if result["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
