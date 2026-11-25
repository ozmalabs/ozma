# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for ``scripts/migrate_display_packs.py``.

The tests use a legacy fixture that predates the extended asset-pack manifest,
then check field preservation, hash/size correctness, dry-run behaviour,
backup+atomic write, idempotence, path safety, invalid JSON handling, and the
real first-party ``retro-gauges`` pack.
"""

from __future__ import annotations

import hashlib
import importlib.util
import json
from pathlib import Path
from types import ModuleType

import pytest

pytestmark = pytest.mark.unit

ROOT = Path(__file__).resolve().parents[3]
SCRIPT = ROOT / "scripts" / "migrate_display_packs.py"
LICENCE_GATE = ROOT / "scripts" / "check_display_licenses.py"
RETRO_DIR = ROOT / "controller" / "widget_packs" / "retro-gauges"
RETRO_MANIFEST = RETRO_DIR / "manifest.json"
BACKUP_NAME = ".manifest.json.bak"


@pytest.fixture(scope="module")
def migrator() -> ModuleType:
    spec = importlib.util.spec_from_file_location(
        "migrate_display_packs", SCRIPT
    )
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


@pytest.fixture(scope="module")
def licence_gate() -> ModuleType:
    spec = importlib.util.spec_from_file_location(
        "check_display_licenses", LICENCE_GATE
    )
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def _sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _write_manifest(path: Path, document: dict) -> bytes:
    """Write a deliberately unsorted, non-canonical manifest."""
    payload = (json.dumps(document, indent=2) + "\n").encode("utf-8")
    path.write_bytes(payload)
    return payload


def _legacy_pack(
    root: Path,
    *,
    pack_id: str = "legacy-pack",
    with_preview: bool = True,
) -> tuple[Path, Path, bytes, dict[str, bytes]]:
    pack_dir = root / pack_id
    pack_dir.mkdir(parents=True, exist_ok=True)
    payloads = {
        "widget.js": b"// never executed by migration\n",
        "widgets/demo.json": b'{"type":"demo"}\n',
    }
    if with_preview:
        payloads["preview.png"] = b"\x89PNG\r\n\x1a\npreview"
    for name, data in payloads.items():
        target = pack_dir / name
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(data)

    manifest = {
        "id": pack_id,
        "name": "Legacy Pack",
        "version": "0.9.0",
        "author": "tester",
        "description": "A legacy v1 widget pack",
        "widget_types": ["demo"],
        "options": [
            {
                "key": "colour",
                "label": "Colour",
                "type": "color",
                "default": "#aabbcc",
                "unknown_option": "keep-me",
            }
        ],
        "tags": ["legacy", "demo"],
        "custom_block": {"nested": [1, 2, 3], "flag": True},
        "assets": [
            {
                "path": "widgets/demo.json",
                "sha256": "0" * 64,
                "size": 999,
                "mime": "application/json",
                "executable": False,
                "sandbox": None,
                "custom_asset": "keep-me",
            }
        ],
        "checksums": {
            "algorithm": "sha256",
            "files": {"widgets/demo.json": "0" * 64},
            "custom_checksum": "keep-me",
        },
    }
    manifest_path = pack_dir / "manifest.json"
    original = _write_manifest(manifest_path, manifest)
    return pack_dir, manifest_path, original, payloads


def _load(path: Path) -> dict:
    return json.loads(path.read_text(encoding="utf-8"))


def test_legacy_manifest_is_migrated_without_losing_fields(migrator, tmp_path):
    pack_dir, manifest_path, _original, payloads = _legacy_pack(tmp_path)

    report = migrator.migrate_root(tmp_path, write=True)

    assert report == {"checked": 1, "changed": 1, "skipped": 0, "errors": []}
    data = _load(manifest_path)

    assert data["type"] == "widget_pack"
    assert data["schema"] == "org.ozma.asset-pack/1"
    assert data["license"] == ""
    assert data["provenance"] == {
        "type": "community",
        "source": "tester",
        "author": "tester",
        "method": "unknown",
        "notes": (
            "Migrated by scripts/migrate_display_packs.py; "
            "review provenance before publishing."
        ),
    }
    assert data["confidence"] == "template"
    assert data["capabilities"] == ["rendered_widget"]
    assert data["permissions"] == []
    assert data["preview"] == "preview.png"

    # Unknown fields and option values survive verbatim.
    assert data["widget_types"] == ["demo"]
    assert data["options"][0]["unknown_option"] == "keep-me"
    assert data["custom_block"] == {"nested": [1, 2, 3], "flag": True}

    assets = {asset["path"]: asset for asset in data["assets"]}
    assert sorted(assets) == ["preview.png", "widget.js", "widgets/demo.json"]
    for path, asset in assets.items():
        assert asset["sha256"] == _sha256(payloads[path])
        assert asset["size"] == len(payloads[path])
        assert asset["mime"]
    assert assets["widget.js"]["executable"] is True
    assert assets["widget.js"]["sandbox"] == "iframe"
    assert assets["widgets/demo.json"]["custom_asset"] == "keep-me"
    assert assets["widgets/demo.json"]["sha256"] == _sha256(
        payloads["widgets/demo.json"]
    )
    assert assets["widgets/demo.json"]["size"] == len(
        payloads["widgets/demo.json"]
    )

    assert data["checksums"]["algorithm"] == "sha256"
    assert data["checksums"]["custom_checksum"] == "keep-me"
    assert data["checksums"]["files"] == {
        path: _sha256(payload) for path, payload in payloads.items()
    }
    # The legacy checksums.files value was stale and is recomputed.
    assert data["checksums"]["files"]["widgets/demo.json"] != "0" * 64


def test_output_is_deterministic_and_trailing_newline(migrator, tmp_path):
    _pack_dir, manifest_path, _original, _payloads = _legacy_pack(tmp_path)

    migrator.migrate_root(tmp_path, write=True)

    raw = manifest_path.read_bytes()
    data = json.loads(raw.decode("utf-8"))
    expected = (
        json.dumps(
            data,
            indent=2,
            sort_keys=True,
            ensure_ascii=False,
        )
        + "\n"
    ).encode("utf-8")
    assert raw == expected
    assert raw.endswith(b"\n")
    assert list(data) == sorted(data)


def test_dry_run_is_default_and_writes_nothing(migrator, tmp_path):
    _pack_dir, manifest_path, original, _payloads = _legacy_pack(tmp_path)

    report = migrator.migrate_root(tmp_path)

    assert set(report) == {"checked", "changed", "skipped", "errors"}
    assert report["checked"] == 1
    assert report["changed"] == 1
    assert report["skipped"] == 0
    assert report["errors"] == []
    assert manifest_path.read_bytes() == original
    assert not (manifest_path.parent / BACKUP_NAME).exists()


def test_write_with_backup_and_idempotence(migrator, tmp_path):
    _pack_dir, manifest_path, original, _payloads = _legacy_pack(tmp_path)

    first = migrator.migrate_root(tmp_path, write=True, backup=True)
    assert first["changed"] == 1
    backup = manifest_path.parent / BACKUP_NAME
    assert backup.read_bytes() == original
    migrated = manifest_path.read_bytes()
    assert migrated != original

    second = migrator.migrate_root(tmp_path, write=True, backup=True)
    assert second == {
        "checked": 1,
        "changed": 0,
        "skipped": 1,
        "errors": [],
    }
    assert manifest_path.read_bytes() == migrated
    # A no-op second run must not clobber the first backup.
    assert backup.read_bytes() == original


def test_preview_is_only_kept_when_the_file_exists(migrator, tmp_path):
    _pack_dir, manifest_path, _original, _payloads = _legacy_pack(
        tmp_path, with_preview=False
    )
    manifest = _load(manifest_path)
    manifest["preview"] = "preview.png"
    _write_manifest(manifest_path, manifest)

    migrator.migrate_root(tmp_path, write=True)

    data = _load(manifest_path)
    assert "preview" not in data
    assert sorted(asset["path"] for asset in data["assets"]) == [
        "widget.js",
        "widgets/demo.json",
    ]


def test_symlink_payload_is_rejected_without_writing(migrator, tmp_path):
    outside = tmp_path / "outside.txt"
    outside.write_text("secret", encoding="utf-8")
    pack_dir, manifest_path, original, _payloads = _legacy_pack(tmp_path)
    (pack_dir / "linked.json").symlink_to(outside)

    report = migrator.migrate_root(tmp_path, write=True)

    assert report["changed"] == 0
    assert any("symlink payload" in error for error in report["errors"])
    assert manifest_path.read_bytes() == original


def test_path_traversal_is_rejected_without_writing(migrator, tmp_path):
    pack_dir, manifest_path, original, _payloads = _legacy_pack(tmp_path)
    manifest = _load(manifest_path)
    manifest["assets"].append({
        "path": "../outside.json",
        "sha256": "0" * 64,
        "size": 1,
    })
    _write_manifest(manifest_path, manifest)
    original = manifest_path.read_bytes()

    report = migrator.migrate_root(tmp_path, write=True)

    assert report["changed"] == 0
    assert any("unsafe asset path" in error for error in report["errors"])
    assert manifest_path.read_bytes() == original


def test_invalid_json_fails_closed(migrator, tmp_path, capsys):
    pack_dir = tmp_path / "broken-pack"
    pack_dir.mkdir()
    manifest_path = pack_dir / "manifest.json"
    manifest_path.write_text("{not valid json", encoding="utf-8")

    report = migrator.migrate_root(tmp_path, write=True)
    assert report["checked"] == 1
    assert report["changed"] == 0
    assert any("invalid JSON" in error for error in report["errors"])
    assert manifest_path.read_text(encoding="utf-8") == "{not valid json"

    assert migrator.main(["--root", str(tmp_path)]) == 1
    printed = json.loads(capsys.readouterr().out)
    assert printed["errors"]


def test_newer_schema_requires_force(migrator, tmp_path):
    pack_dir, manifest_path, _original, _payloads = _legacy_pack(tmp_path)
    manifest = _load(manifest_path)
    manifest["schema"] = "org.ozma.asset-pack/2"
    manifest["type"] = "theme_pack"
    _write_manifest(manifest_path, manifest)
    original = manifest_path.read_bytes()

    report = migrator.migrate_root(tmp_path, write=True)
    assert report["changed"] == 0
    assert any("newer or unknown" in error for error in report["errors"])
    assert manifest_path.read_bytes() == original

    forced = migrator.migrate_root(tmp_path, write=True, force=True)
    assert forced["changed"] == 1
    migrated = _load(manifest_path)
    assert migrated["schema"] == "org.ozma.asset-pack/1"
    assert migrated["type"] == "theme_pack"


def test_real_retro_gauges_is_already_migrated_and_gate_clean(
    migrator, licence_gate
):
    original = RETRO_MANIFEST.read_bytes()

    report = migrator.migrate_root(RETRO_DIR.parent, write=True)

    assert report == {
        "checked": 1,
        "changed": 0,
        "skipped": 1,
        "errors": [],
    }
    assert RETRO_MANIFEST.read_bytes() == original

    outcome = licence_gate.check_manifest(
        RETRO_MANIFEST,
        root=ROOT,
        mode="local",
        first_party_author="ozmalabs",
    )
    assert outcome["errors"] == []
