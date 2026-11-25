# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for the display licence gate script."""

from __future__ import annotations

import hashlib
import importlib.util
import json
from pathlib import Path
from types import ModuleType

import pytest

pytestmark = pytest.mark.unit

ROOT = Path(__file__).resolve().parents[3]
SCRIPT = ROOT / "scripts" / "check_display_licenses.py"
RETRO_MANIFEST = (
    ROOT / "controller" / "widget_packs" / "retro-gauges" / "manifest.json"
)


@pytest.fixture(scope="module")
def gate() -> ModuleType:
    spec = importlib.util.spec_from_file_location(
        "check_display_licenses", SCRIPT
    )
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def _sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _write_pack(
    root: Path,
    pack_id: str,
    manifest: dict,
    payload: dict[str, bytes] | None = None,
) -> Path:
    pack_dir = root / "controller" / "widget_packs" / pack_id
    pack_dir.mkdir(parents=True, exist_ok=True)
    for name, data in (payload or {}).items():
        target = pack_dir / name
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(data)
    document = {"id": pack_id, "version": "1.0.0"}
    document.update(manifest)
    manifest_path = pack_dir / "manifest.json"
    manifest_path.write_text(
        json.dumps(document, indent=2, sort_keys=True), encoding="utf-8"
    )
    return manifest_path


def _asset(path: str, data: bytes, **extra) -> dict:
    entry = {
        "path": path,
        "sha256": _sha256(data),
        "size": len(data),
    }
    entry.update(extra)
    return entry


def test_allowed_licences_pass_in_published_mode(gate, tmp_path):
    payload = {"widget.json": b'{"type":"demo"}'}
    for index, license_name in enumerate(sorted(gate.ALLOWED_LICENSES)):
        _write_pack(
            tmp_path,
            f"allowed-{index}",
            {
                "license": license_name,
                "author": "tester",
                "assets": [_asset("widget.json", payload["widget.json"])],
            },
            payload,
        )

    result = gate.check(root=tmp_path, mode="published")
    assert result["ok"], result["errors"]
    assert len(result["checked"]) == len(gate.ALLOWED_LICENSES)


@pytest.mark.parametrize(
    "license_name",
    [
        "GPL-2.0-only",
        "GPL-3.0-or-later",
        "CC-BY-NC-4.0",
        "personal-use-only",
        "Unknown-1.0",
    ],
)
def test_disallowed_and_unknown_licences_fail_in_both_modes(
    gate, tmp_path, license_name
):
    _write_pack(
        tmp_path,
        "bad-pack",
        {"license": license_name, "author": "third-party"},
    )

    local = gate.check(root=tmp_path, mode="local")
    published = gate.check(root=tmp_path, mode="published")

    assert local["ok"] is False
    assert published["ok"] is False
    assert local["errors"]
    assert published["errors"]


def test_missing_licence_is_an_error(gate, tmp_path):
    _write_pack(tmp_path, "no-license", {"author": "tester"})
    result = gate.check(root=tmp_path, mode="local")
    assert result["ok"] is False
    assert any("missing licence" in error for error in result["errors"])


def test_agpl_passes_local_only_for_first_party(gate, tmp_path):
    _write_pack(
        tmp_path,
        "first-party",
        {
            "license": "AGPL-3.0-only WITH OzmaPluginException",
            "author": "ozmalabs",
        },
    )

    local = gate.check(root=tmp_path, mode="local")
    published = gate.check(root=tmp_path, mode="published")

    assert local["ok"], local["errors"]
    assert published["ok"] is False
    assert any("AGPL" in error for error in published["errors"])


def test_agpl_first_party_can_come_from_provenance(gate, tmp_path):
    _write_pack(
        tmp_path,
        "provenance-first-party",
        {
            "license": "AGPL-3.0-only",
            "provenance": {"author": "ozmalabs"},
        },
    )
    result = gate.check(root=tmp_path, mode="local")
    assert result["ok"], result["errors"]


def test_agpl_third_party_fails_local_mode(gate, tmp_path):
    _write_pack(
        tmp_path,
        "third-party",
        {"license": "AGPL-3.0-only", "author": "someone-else"},
    )
    result = gate.check(root=tmp_path, mode="local")
    assert result["ok"] is False
    assert any("first-party" in error for error in result["errors"])


def test_verified_hash_passes(gate, tmp_path):
    data = b'{"type":"hashed"}\n'
    _write_pack(
        tmp_path,
        "hashed-pack",
        {
            "license": "MIT",
            "author": "tester",
            "assets": [_asset("widgets/hashed.json", data)],
        },
        {"widgets/hashed.json": data},
    )
    result = gate.check(root=tmp_path, mode="published")
    assert result["ok"], result["errors"]
    assert result["checked"][0]["hashes_verified"] == 1


def test_hash_mismatch_fails(gate, tmp_path):
    data = b'{"type":"hashed"}\n'
    _write_pack(
        tmp_path,
        "bad-hash",
        {
            "license": "MIT",
            "author": "tester",
            "assets": [
                {
                    "path": "widgets/hashed.json",
                    "sha256": "0" * 64,
                }
            ],
        },
        {"widgets/hashed.json": data},
    )
    result = gate.check(root=tmp_path, mode="local")
    assert result["ok"] is False
    assert any("SHA-256 mismatch" in error for error in result["errors"])


def test_missing_declared_asset_fails(gate, tmp_path):
    _write_pack(
        tmp_path,
        "missing-asset",
        {
            "license": "MIT",
            "author": "tester",
            "assets": [{"path": "missing.json", "sha256": "a" * 64}],
        },
    )
    result = gate.check(root=tmp_path, mode="local")
    assert result["ok"] is False
    assert any("declared asset missing" in error for error in result["errors"])


def test_checksums_map_is_verified(gate, tmp_path):
    data = b"theme token data\n"
    _write_pack(
        tmp_path,
        "checksum-map",
        {
            "license": "Apache-2.0",
            "author": "tester",
            "checksums": {"files": {"theme.json": _sha256(data)}},
        },
        {"theme.json": data},
    )
    result = gate.check(root=tmp_path, mode="published")
    assert result["ok"], result["errors"]
    assert result["checked"][0]["hashes_verified"] == 1


@pytest.mark.parametrize("sandbox", [None, "", "none", "None"])
def test_executable_asset_requires_sandbox(gate, tmp_path, sandbox):
    data = b"console.log('never execute me')\n"
    _write_pack(
        tmp_path,
        "exec-no-sandbox",
        {
            "license": "MIT",
            "author": "tester",
            "assets": [
                _asset("widget.js", data, executable=True, sandbox=sandbox)
            ],
        },
        {"widget.js": data},
    )
    result = gate.check(root=tmp_path, mode="local")
    assert result["ok"] is False
    assert any("no sandbox" in error for error in result["errors"])


def test_executable_asset_with_sandbox_passes(gate, tmp_path):
    data = b"console.log('sandboxed')\n"
    _write_pack(
        tmp_path,
        "exec-sandboxed",
        {
            "license": "MIT",
            "author": "tester",
            "assets": [
                _asset("widget.js", data, executable=True, sandbox="iframe")
            ],
        },
        {"widget.js": data},
    )
    result = gate.check(root=tmp_path, mode="local")
    assert result["ok"], result["errors"]


def test_invalid_json_is_an_error(gate, tmp_path):
    pack_dir = tmp_path / "controller" / "widget_packs" / "broken"
    pack_dir.mkdir(parents=True)
    (pack_dir / "manifest.json").write_text("{not json", encoding="utf-8")

    result = gate.check(root=tmp_path, mode="local")
    assert result["ok"] is False
    assert any("invalid JSON" in error for error in result["errors"])


def test_json_output_is_deterministic_and_exit_codes(gate, tmp_path, capsys):
    _write_pack(
        tmp_path,
        "zeta-pack",
        {"license": "MIT", "author": "tester"},
    )
    _write_pack(
        tmp_path,
        "alpha-pack",
        {"license": "BSD-3-Clause", "author": "tester"},
    )

    first_code = gate.main(["--root", str(tmp_path), "--json"])
    first = capsys.readouterr().out
    second_code = gate.main(["--root", str(tmp_path), "--json"])
    second = capsys.readouterr().out

    assert first_code == 0
    assert second_code == 0
    assert first == second
    payload = json.loads(first)
    assert set(payload) == {"ok", "errors", "warnings", "checked"}
    assert payload["ok"] is True
    assert [item["manifest"] for item in payload["checked"]] == [
        "controller/widget_packs/alpha-pack/manifest.json",
        "controller/widget_packs/zeta-pack/manifest.json",
    ]

    _write_pack(
        tmp_path,
        "gpl-pack",
        {"license": "GPL-2.0-only", "author": "tester"},
    )
    failed_code = gate.main(["--root", str(tmp_path), "--mode", "published"])
    capsys.readouterr()
    assert failed_code == 1


def test_real_retro_gauges_passes_local_and_fails_published(gate):
    local = gate.check_manifest(
        RETRO_MANIFEST,
        root=ROOT,
        mode="local",
        first_party_author="ozmalabs",
    )
    published = gate.check_manifest(
        RETRO_MANIFEST,
        root=ROOT,
        mode="published",
        first_party_author="ozmalabs",
    )

    assert local["errors"] == []
    assert published["errors"]
    assert any("AGPL" in error for error in published["errors"])
