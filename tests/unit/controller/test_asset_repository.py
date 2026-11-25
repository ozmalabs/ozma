# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for the local display asset repository."""

from __future__ import annotations

import hashlib
import io
import json
import tarfile
import zipfile
from pathlib import Path
from types import SimpleNamespace

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

from asset_repository import (
    ASSET_SCHEMA,
    AssetCache,
    AssetConflictError,
    AssetHashError,
    AssetIndex,
    AssetNotFoundError,
    AssetPackEntry,
    AssetRepository,
    AssetSignatureError,
    AssetSyncPolicy,
    InvalidAssetError,
    RepositoryLimits,
    UnsafeArchiveError,
    build_router,
)

pytestmark = pytest.mark.unit

ROOT = Path(__file__).resolve().parents[3]
RETRO_MANIFEST = ROOT / "controller" / "widget_packs" / "retro-gauges" / "manifest.json"


# ── Fixture helpers ─────────────────────────────────────────────────────────


def _asset_entry(path: str, data: bytes) -> dict:
    return {
        "path": path,
        "sha256": hashlib.sha256(data).hexdigest(),
        "size": len(data),
        "mime": "application/javascript" if path.endswith(".js") else "application/json",
        "executable": path.endswith(".js"),
        "sandbox": "iframe" if path.endswith(".js") else "none",
    }


def _pack(
    pack_id: str = "demo-pack",
    version: str = "1.0.0",
    *,
    payload: dict[str, bytes] | None = None,
    declare: bool = True,
    **overrides,
) -> tuple[dict[str, bytes], dict]:
    payload = dict(
        payload
        if payload is not None
        else {
            "renderer.js": b"// this file must never be executed by the repository\n",
            "widgets/demo.json": b'{"type":"demo"}\n',
        }
    )
    manifest = {
        "id": pack_id,
        "type": "widget_pack",
        "name": "Demo Pack",
        "version": version,
        "schema": ASSET_SCHEMA,
        "author": "tester",
        "description": "A demo asset pack",
        "license": "MIT",
        "source_url": "https://example.test/demo-pack",
        "tags": ["demo", "retro"],
        "capabilities": ["rendered_widget"],
        "permissions": [],
        "min_ozma_version": "1.0.0",
        "min_display_schema": "org.ozma.display/1",
        "provenance": {"type": "original", "author": "tester"},
        "confidence": "exact",
        "vibe": {"keywords": ["warm"]},
        "shape": {"family": "arc"},
        "coherence": {"score": 0.9},
        "published_at": "2026-01-01T00:00:00Z",
        "yanked": False,
    }
    if declare:
        manifest["assets"] = [
            _asset_entry(name, data) for name, data in sorted(payload.items())
        ]
    manifest.update(overrides)
    files = dict(payload)
    files["manifest.json"] = json.dumps(manifest, sort_keys=True).encode("utf-8")
    return files, manifest


def _rewrite_manifest(files: dict[str, bytes], manifest: dict) -> dict[str, bytes]:
    files = dict(files)
    files["manifest.json"] = json.dumps(manifest, sort_keys=True).encode("utf-8")
    return files


def _write_pack(root: Path, files: dict[str, bytes]) -> Path:
    for name, data in files.items():
        target = root / name
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(data)
    return root


def _zip_bytes(
    files: dict[str, bytes],
    *,
    extras: list[tuple[str, bytes, int]] | None = None,
) -> bytes:
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w", zipfile.ZIP_DEFLATED) as archive:
        for name, data in files.items():
            info = zipfile.ZipInfo(name, date_time=(2020, 1, 1, 0, 0, 0))
            info.external_attr = 0o100644 << 16
            archive.writestr(info, data)
        for name, data, mode in extras or []:
            info = zipfile.ZipInfo(name, date_time=(2020, 1, 1, 0, 0, 0))
            info.external_attr = mode << 16
            archive.writestr(info, data)
    return buffer.getvalue()


def _tar_bytes(
    files: dict[str, bytes],
    *,
    extras: list[tuple[str, bytes, bytes]] | None = None,
) -> bytes:
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w:gz") as archive:
        for name, data in files.items():
            info = tarfile.TarInfo(name)
            info.size = len(data)
            info.mtime = 1_600_000_000
            archive.addfile(info, io.BytesIO(data))
        for name, kind, linkname in extras or []:
            info = tarfile.TarInfo(name)
            info.type = kind
            info.linkname = linkname
            archive.addfile(info)
    return buffer.getvalue()


def _repo(tmp_path: Path, **kwargs) -> AssetRepository:
    return AssetRepository(
        packs_dir=tmp_path / "widget_packs",
        cache_dir=tmp_path / "asset_cache",
        **kwargs,
    )


def _client(repository: AssetRepository) -> TestClient:
    app = FastAPI()
    app.include_router(build_router(repository))
    return TestClient(app)


def _index_payload(*manifests: dict, **extra) -> dict:
    payload = {"schema": "org.ozma.asset-index/1", "packs": list(manifests)}
    payload.update(extra)
    return payload


# ── Manifest parsing ────────────────────────────────────────────────────────


def test_existing_retro_gauges_manifest_loads_and_preserves_unknown_fields():
    raw = json.loads(RETRO_MANIFEST.read_text(encoding="utf-8"))
    entry = AssetPackEntry.from_manifest(raw)

    assert entry.id == "retro-gauges"
    assert entry.type == "widget_pack"
    assert entry.version == "1.0.0"
    assert entry.name == "Retro Analog Gauges"
    assert entry.license == "AGPL-3.0-only WITH OzmaPluginException"
    assert entry.provenance["type"] == "original"
    assert entry.permissions == []
    assert entry.capabilities == ["rendered_widget", "native_render"]
    assert entry.assets == []

    # Fields the older parser did not model are retained verbatim.
    assert entry.extra["widget_types"] == raw["widget_types"]
    assert entry.extra["options"] == raw["options"]
    assert entry.extra["themes"] == raw["themes"]
    assert entry.extra["ozma_connect_url"] == raw["ozma_connect_url"]
    assert entry.extra["checksums"] == raw["checksums"]

    round_trip = entry.to_manifest()
    assert round_trip["options"] == raw["options"]
    assert round_trip["widget_types"] == raw["widget_types"]
    assert round_trip["checksums"] == raw["checksums"]


def test_unknown_manifest_fields_survive_round_trip():
    _, manifest = _pack(x_future={"nested": [1, 2, 3]}, future_flag=True)
    files = _rewrite_manifest({}, manifest)
    entry, _ = __import__("asset_repository")._parse_manifest(files)

    assert entry.extra["x_future"] == {"nested": [1, 2, 3]}
    assert entry.extra["future_flag"] is True
    assert entry.to_manifest()["x_future"] == {"nested": [1, 2, 3]}


def test_manifest_requires_id_and_parses_asset_aliases():
    with pytest.raises(InvalidAssetError):
        AssetPackEntry.from_manifest({"name": "missing id"})

    entry = AssetPackEntry.from_manifest(
        {
            "id": "alias-pack",
            "assets": [
                {
                    "path": "a.bin",
                    "sha256": "a" * 64,
                    "size_bytes": 3,
                    "mime_type": "application/octet-stream",
                    "custom": "kept",
                }
            ],
        }
    )
    asset = entry.assets[0]
    assert asset.size == 3
    assert asset.mime == "application/octet-stream"
    assert asset.extra == {"custom": "kept"}
    assert asset.to_dict()["path"] == "a.bin"


# ── Install / update / rollback ─────────────────────────────────────────────


def test_install_update_and_rollback_preserve_options(tmp_path):
    repository = _repo(tmp_path)
    v1_files, _ = _pack(version="1.0.0", payload={"renderer.js": b"v1\n"})
    first = repository.install(_write_pack(tmp_path / "src-v1", v1_files))
    assert first.version == "1.0.0"
    assert (tmp_path / "widget_packs" / "demo-pack" / "renderer.js").read_bytes() == b"v1\n"

    options_path = tmp_path / "widget_packs" / "demo-pack" / "options.json"
    options_path.write_text('{"bezel_color": "#123456"}', encoding="utf-8")

    v2_files, _ = _pack(version="2.0.0", payload={"renderer.js": b"v2\n"})
    second = repository.install(_write_pack(tmp_path / "src-v2", v2_files))
    assert second.version == "2.0.0"
    assert (tmp_path / "widget_packs" / "demo-pack" / "renderer.js").read_bytes() == b"v2\n"
    assert json.loads(options_path.read_text(encoding="utf-8")) == {
        "bezel_color": "#123456"
    }

    state = repository.cache.load_installed()["packs"]["demo-pack"]
    assert state["current"] == "2.0.0"
    assert state["previous"] == "1.0.0"

    rolled_back = repository.rollback("demo-pack")
    assert rolled_back.version == "1.0.0"
    assert (tmp_path / "widget_packs" / "demo-pack" / "renderer.js").read_bytes() == b"v1\n"
    assert json.loads(options_path.read_text(encoding="utf-8")) == {
        "bezel_color": "#123456"
    }

    # Both versions remain available in the offline cache.
    assert (tmp_path / "asset_cache" / "packs" / "demo-pack" / "1.0.0").is_dir()
    assert (tmp_path / "asset_cache" / "packs" / "demo-pack" / "2.0.0").is_dir()


def test_install_zip_and_tar_archives(tmp_path):
    repository = _repo(tmp_path)
    files, _ = _pack("zip-pack")
    entry = repository.install(_zip_bytes(files))
    assert entry.id == "zip-pack"
    assert (tmp_path / "widget_packs" / "zip-pack" / "manifest.json").is_file()

    tar_files, _ = _pack("tar-pack")
    entry = repository.install(_tar_bytes(tar_files))
    assert entry.id == "tar-pack"
    assert (tmp_path / "widget_packs" / "tar-pack" / "widgets" / "demo.json").is_file()


def test_archive_with_single_top_level_directory_is_normalized(tmp_path):
    files, _ = _pack("nested-pack")
    nested = {f"nested-pack/{name}": data for name, data in files.items()}
    repository = _repo(tmp_path)
    entry = repository.install(_zip_bytes(nested))
    assert entry.id == "nested-pack"
    assert (tmp_path / "widget_packs" / "nested-pack" / "manifest.json").is_file()


def test_hash_mismatch_and_undeclared_files_fail_closed(tmp_path):
    repository = _repo(tmp_path)
    files, manifest = _pack()
    manifest["assets"][0]["sha256"] = "0" * 64
    with pytest.raises(AssetHashError):
        repository.install(_write_pack(tmp_path / "bad-hash", _rewrite_manifest(files, manifest)))

    files, manifest = _pack()
    files["smuggled.bin"] = b"not declared"
    with pytest.raises(InvalidAssetError, match="undeclared"):
        repository.install(_write_pack(tmp_path / "undeclared", files))

    assert not (tmp_path / "widget_packs" / "demo-pack").exists()


def test_checksums_json_is_verified_when_manifest_assets_are_absent(tmp_path):
    repository = _repo(tmp_path)
    files, manifest = _pack("checksum-pack", declare=False)
    checksums = {
        "files": {
            name: hashlib.sha256(data).hexdigest()
            for name, data in files.items()
        }
    }
    files["checksums.json"] = json.dumps(checksums).encode("utf-8")
    entry = repository.install(_write_pack(tmp_path / "checksum-src", files))
    assert entry.id == "checksum-pack"

    bad_files, _ = _pack("checksum-bad", declare=False)
    bad_checksums = {
        "files": {
            name: hashlib.sha256(data).hexdigest()
            for name, data in bad_files.items()
        }
    }
    bad_checksums["files"]["renderer.js"] = "0" * 64
    bad_files["checksums.json"] = json.dumps(bad_checksums).encode("utf-8")
    with pytest.raises(AssetHashError):
        repository.install(
            _write_pack(tmp_path / "checksum-bad-src", bad_files)
        )


def test_path_traversal_absolute_and_unsafe_archives_are_rejected(tmp_path):
    repository = _repo(tmp_path)
    files, _ = _pack()
    for bad_name in ("../escape.txt", "/tmp/absolute.txt", "a/../../escape.txt"):
        archive = _zip_bytes(files, extras=[(bad_name, b"x", 0o100644)])
        with pytest.raises(UnsafeArchiveError):
            repository.install(archive)

    # Zip symlink.
    archive = _zip_bytes(files, extras=[("link", b"../../etc/passwd", 0o120777)])
    with pytest.raises(UnsafeArchiveError):
        repository.install(archive)

    # Tar symlink and hardlink.
    archive = _tar_bytes(files, extras=[("link", tarfile.SYMTYPE, "manifest.json")])
    with pytest.raises(UnsafeArchiveError):
        repository.install(archive)
    archive = _tar_bytes(files, extras=[("hard", tarfile.LNKTYPE, "manifest.json")])
    with pytest.raises(UnsafeArchiveError):
        repository.install(archive)


def test_directory_symlink_and_depth_limits_are_rejected(tmp_path):
    repository = _repo(tmp_path)
    root = _write_pack(tmp_path / "symlink-src", _pack()[0])
    (root / "link").symlink_to(root / "manifest.json")
    with pytest.raises(UnsafeArchiveError):
        repository.install(root)

    deep_repo = _repo(tmp_path / "deep", limits=RepositoryLimits(max_depth=2))
    deep_files, _ = _pack("deep-pack", payload={"a/b/c/d.txt": b"too deep"})
    with pytest.raises(UnsafeArchiveError, match="depth"):
        deep_repo.install(_write_pack(tmp_path / "deep-src", deep_files))


def test_oversize_file_and_count_limits_are_rejected(tmp_path):
    repository = _repo(
        tmp_path,
        limits=RepositoryLimits(max_file_bytes=32, max_total_bytes=64),
    )
    files, _ = _pack("big-pack", payload={"big.bin": b"x" * 128})
    with pytest.raises(UnsafeArchiveError, match="size limit"):
        repository.install(_write_pack(tmp_path / "big-src", files))

    count_repo = _repo(
        tmp_path / "count",
        limits=RepositoryLimits(max_files=2),
    )
    count_files, _ = _pack(
        "many-pack",
        payload={"a.bin": b"a", "b.bin": b"b", "c.bin": b"c"},
    )
    with pytest.raises(UnsafeArchiveError, match="count"):
        count_repo.install(_write_pack(tmp_path / "count-src", count_files))


def test_yanked_versions_are_not_auto_installable(tmp_path):
    repository = _repo(tmp_path)
    files, _ = _pack("yanked-pack", yanked=True)
    source = _write_pack(tmp_path / "yanked-src", files)

    with pytest.raises(AssetConflictError, match="yanked"):
        repository.install(source, auto=True)

    entry = repository.install(source)
    assert entry.yanked is True

    index = AssetIndex()
    index.add(entry)
    repository.index = index
    with pytest.raises(AssetConflictError, match="yanked"):
        repository.install_from_index("yanked-pack", auto=True)


def test_index_latest_skips_yanked_but_keeps_older_good_version():
    index = AssetIndex()
    index.add(AssetPackEntry.from_manifest(_pack("multi", "1.0.0")[1]))
    index.add(AssetPackEntry.from_manifest(_pack("multi", "2.0.0", yanked=True)[1]))

    assert index.latest("multi").version == "1.0.0"
    assert index.latest("multi", include_yanked=True).version == "2.0.0"
    assert [item.version for item in index.versions("multi")] == ["2.0.0", "1.0.0"]


# ── Signatures ──────────────────────────────────────────────────────────────


def test_signature_present_without_verifier_fails_closed(tmp_path):
    repository = _repo(tmp_path)
    files, _ = _pack(signature="detached-signature")
    with pytest.raises(AssetSignatureError, match="no verifier"):
        repository.install(_write_pack(tmp_path / "signed-src", files))
    assert not (tmp_path / "widget_packs" / "demo-pack").exists()


def test_signature_verifier_accepts_and_rejects(tmp_path):
    seen: list[str] = []

    def good_verifier(manifest_bytes: bytes, signature: str, files) -> bool:
        seen.append(signature)
        return manifest_bytes and signature == "valid" and "manifest.json" in files

    repository = _repo(tmp_path, signature_verifier=good_verifier)
    files, _ = _pack(signature="valid")
    entry = repository.install(_write_pack(tmp_path / "good-src", files))
    assert entry.version == "1.0.0"
    assert seen == ["valid"]

    bad_repository = _repo(tmp_path / "bad", signature_verifier=good_verifier)
    bad_files, _ = _pack("bad-pack", signature="invalid")
    with pytest.raises(AssetSignatureError, match="verification failed"):
        bad_repository.install(_write_pack(tmp_path / "bad-src", bad_files))


def test_unsigned_policy_and_verifier_exceptions_fail_closed(tmp_path):
    files, _ = _pack("unsigned-pack")

    require = _repo(tmp_path / "require", require_signature=True)
    with pytest.raises(AssetSignatureError, match="unsigned"):
        require.install(_write_pack(tmp_path / "require-src", files))

    denied = _repo(tmp_path / "denied", allow_unsigned=False)
    with pytest.raises(AssetSignatureError, match="unsigned"):
        denied.install(_write_pack(tmp_path / "denied-src", files))

    def exploding_verifier(manifest_bytes, signature, files):
        raise RuntimeError("verifier exploded")

    exploding = _repo(tmp_path / "explode", signature_verifier=exploding_verifier)
    signed, _ = _pack("explode-pack", signature="sig")
    with pytest.raises(AssetSignatureError, match="verification failed"):
        exploding.install(_write_pack(tmp_path / "explode-src", signed))


def test_two_argument_signature_verifier_is_supported(tmp_path):
    repository = _repo(
        tmp_path,
        signature_verifier=lambda manifest_bytes, signature: signature == "ok",
    )
    files, _ = _pack("short-verifier", signature="ok")
    entry = repository.install(_write_pack(tmp_path / "short-src", files))
    assert entry.id == "short-verifier"


def test_sync_policy_parsing_and_unknown_field_preservation():
    policy = AssetSyncPolicy.from_dict(
        {
            "mode": "FULL",
            "sync_interval_hours": "12",
            "cache_size_mb": "256",
            "include_previews": "false",
            "include_external_imports": 1,
            "verify_signatures": "true",
            "future_policy_field": {"kept": True},
        }
    )
    assert policy.mode == "full"
    assert policy.sync_interval_hours == 12
    assert policy.cache_size_mb == 256
    assert policy.include_previews is False
    assert policy.include_external_imports is True
    assert policy.verify_signatures is True
    assert policy.extra == {"future_policy_field": {"kept": True}}
    assert policy.to_dict()["future_policy_field"] == {"kept": True}

    with pytest.raises(InvalidAssetError):
        AssetSyncPolicy.from_dict({"mode": "sometimes"})
    with pytest.raises(InvalidAssetError):
        AssetSyncPolicy.from_dict({"sync_interval_hours": -1})


# ── Index persistence, search, categories, related, stats ──────────────────


def test_index_save_load_is_atomic_and_preserves_sync_metadata(tmp_path):
    path = tmp_path / "cache" / "index.json"
    index = AssetIndex(path=path)
    index.add(AssetPackEntry.from_manifest(_pack("a-pack", "1.0.0")[1]))
    index.add(AssetPackEntry.from_manifest(_pack("a-pack", "2.0.0")[1]))
    index.last_sync = "2026-11-25T12:00:00+00:00"
    index.etag = '"etag-value"'
    index.save()

    assert path.is_file()
    assert not list(path.parent.glob("*.tmp"))

    loaded = AssetIndex.from_file(path)
    assert loaded.count == 2
    assert loaded.last_sync == "2026-11-25T12:00:00+00:00"
    assert loaded.etag == '"etag-value"'
    assert loaded.content_hash
    assert loaded.entry("a-pack").version == "2.0.0"


def _search_index() -> AssetIndex:
    index = AssetIndex()
    index.add(
        AssetPackEntry.from_manifest(
            _pack(
                "retro-gauges-pack",
                tags=["retro", "gauges"],
                vibe={"keywords": ["warm", "analog"]},
                shape={"family": "arc"},
                permissions=["internet"],
            )[1]
        )
    )
    index.add(
        AssetPackEntry.from_manifest(
            _pack(
                "minimal-clock",
                type="theme_pack",
                name="Minimal Clock",
                tags=["minimal", "clock"],
                vibe={"keywords": ["cool"]},
                shape={"family": "rectangle"},
            )[1]
        )
    )
    index.add(
        AssetPackEntry.from_manifest(
            _pack(
                "arc-audio",
                type="theme_pack",
                name="Arc Audio",
                tags=["audio"],
                vibe={"keywords": ["warm", "analog"]},
                shape={"family": "arc"},
            )[1]
        )
    )
    return index


def test_search_by_query_type_tag_and_permission():
    index = _search_index()

    assert [item.id for item in index.search("clock")] == ["minimal-clock"]
    assert [item.id for item in index.search(type="theme_pack")] == [
        "arc-audio",
        "minimal-clock",
    ]
    assert [item.id for item in index.search(tag="retro")] == ["retro-gauges-pack"]
    assert [item.id for item in index.search(permission="internet")] == [
        "retro-gauges-pack"
    ]
    assert index.search(tag="does-not-exist") == []
    with pytest.raises(InvalidAssetError):
        index.search(type="not-a-pack-type")


def test_search_uses_latest_version_and_keywords():
    index = AssetIndex()
    index.add(AssetPackEntry.from_manifest(_pack("evolving", "1.0.0", tags=["old"])[1]))
    index.add(AssetPackEntry.from_manifest(_pack("evolving", "2.0.0", tags=["new"])[1]))

    results = index.search()
    assert len(results) == 1
    assert results[0].version == "2.0.0"
    assert index.search(tag="new")
    assert index.search(tag="old") == []


def test_categories_related_and_stats():
    index = _search_index()

    categories = index.categories()
    assert categories["types"] == {"theme_pack": 2, "widget_pack": 1}
    assert categories["tags"]["retro"] == 1
    assert {"id": "theme_pack", "kind": "type", "count": 2} in categories["categories"]

    related = index.related("retro-gauges-pack")
    assert [item.id for item in related] == ["arc-audio"]

    stats = index.stats()
    assert stats["packs"] == 3
    assert stats["versions"] == 3
    assert stats["by_type"]["theme_pack"] == 2
    assert stats["yanked_versions"] == 0
    assert stats["content_hash"]

    with pytest.raises(AssetNotFoundError):
        index.related("missing-pack")


# ── Sync ────────────────────────────────────────────────────────────────────


def test_offline_sync_never_calls_transport(tmp_path):
    def exploding_transport(*args, **kwargs):
        raise AssertionError("offline sync must not touch the network")

    repository = _repo(
        tmp_path,
        policy=AssetSyncPolicy(mode="offline"),
        transport=exploding_transport,
    )
    result = repository.sync("https://example.test/index.json")
    assert result["offline"] is True
    assert result["synced"] is False
    assert result["cached"] is True
    assert result["mode"] == "offline"


def test_sync_with_injected_transport_updates_cache(tmp_path):
    _, manifest = _pack("remote-pack")
    calls: list[dict] = []

    def transport(url, headers=None):
        calls.append({"url": url, "headers": headers or {}})
        return _index_payload(manifest, etag='"remote-etag"')

    repository = _repo(tmp_path)
    result = repository.sync(
        "https://example.test/index.json",
        transport=transport,
        policy=AssetSyncPolicy(mode="full"),
    )

    assert result["synced"] is True
    assert result["updated"] is True
    assert result["entries"] == 1
    assert calls[0]["url"] == "https://example.test/index.json"
    assert repository.index.entry("remote-pack") is not None
    assert repository.index.etag == '"remote-etag"'
    assert repository.index.last_sync
    assert repository.index.content_hash

    saved = json.loads(repository.cache.index_path.read_text(encoding="utf-8"))
    assert saved["etag"] == '"remote-etag"'
    assert saved["entries"][0]["id"] == "remote-pack"


def test_sync_installed_mode_filters_packs_and_imports(tmp_path):
    repository = _repo(tmp_path)
    repository.install(_write_pack(tmp_path / "installed-src", _pack("installed-pack")[0]))

    _, installed_manifest = _pack("installed-pack")
    _, other_manifest = _pack("other-pack")
    _, import_manifest = _pack("import-pack", type="import_pack")
    payload = _index_payload(installed_manifest, other_manifest, import_manifest)

    repository.sync(
        "https://example.test/index.json",
        transport=lambda url, headers=None: payload,
        policy=AssetSyncPolicy(mode="installed"),
    )

    assert {entry.id for entry in repository.index.entries} == {"installed-pack"}


def test_sync_honours_etag_and_304(tmp_path):
    _, manifest = _pack("etag-pack")
    calls: list[dict] = []

    def first_transport(url, headers=None):
        calls.append(dict(headers or {}))
        return _index_payload(manifest, etag='"v1"')

    repository = _repo(tmp_path)
    repository.sync(
        "https://example.test/index.json",
        transport=first_transport,
        policy=AssetSyncPolicy(mode="full"),
    )
    assert repository.index.etag == '"v1"'

    def second_transport(url, headers=None):
        calls.append(dict(headers or {}))
        return SimpleNamespace(status_code=304, headers={}, content=b"")

    result = repository.sync(
        "https://example.test/index.json",
        transport=second_transport,
        policy=AssetSyncPolicy(mode="full"),
    )
    assert calls[-1]["If-None-Match"] == '"v1"'
    assert result["not_modified"] is True
    assert repository.index.entry("etag-pack") is not None


def test_sync_signature_cases_fail_closed_without_poisoning_cache(tmp_path):
    _, manifest = _pack("signed-index-pack")
    signed_payload = _index_payload(manifest, signature="good", etag='"signed"')

    repository = _repo(
        tmp_path,
        signature_verifier=lambda raw, signature, files: signature == "good",
    )
    result = repository.sync(
        "https://example.test/index.json",
        transport=lambda url, headers=None: signed_payload,
        policy=AssetSyncPolicy(mode="full", verify_signatures=True),
    )
    assert result["synced"] is True
    before = repository.cache.index_path.read_text(encoding="utf-8")

    bad_payload = _index_payload(_pack("poison-pack")[1], signature="bad")
    with pytest.raises(AssetSignatureError):
        repository.sync(
            "https://example.test/index.json",
            transport=lambda url, headers=None: bad_payload,
            policy=AssetSyncPolicy(mode="full", verify_signatures=True),
        )
    assert repository.cache.index_path.read_text(encoding="utf-8") == before
    assert repository.index.entry("signed-index-pack") is not None

    no_verifier = _repo(tmp_path / "no-verifier")
    with pytest.raises(AssetSignatureError, match="no verifier"):
        no_verifier.sync(
            "https://example.test/index.json",
            transport=lambda url, headers=None: signed_payload,
            policy=AssetSyncPolicy(mode="full"),
        )

    unsigned = _repo(tmp_path / "unsigned")
    with pytest.raises(AssetSignatureError, match="unsigned"):
        unsigned.sync(
            "https://example.test/index.json",
            transport=lambda url, headers=None: _index_payload(manifest),
            policy=AssetSyncPolicy(mode="full", verify_signatures=True),
        )


# ── API router ──────────────────────────────────────────────────────────────


def test_router_search_entry_versions_categories_and_stats(tmp_path):
    repository = _repo(tmp_path)
    _, manifest = _pack("api-pack")
    repository.index.add(AssetPackEntry.from_manifest(manifest))
    _, old_manifest = _pack("api-pack", version="0.9.0")
    repository.index.add(AssetPackEntry.from_manifest(old_manifest))
    client = _client(repository)

    response = client.get("/api/v1/asset-db/search", params={"q": "demo"})
    assert response.status_code == 200
    assert response.json()["count"] == 1
    assert response.json()["entries"][0]["id"] == "api-pack"

    response = client.get("/api/v1/asset-db/entry/api-pack")
    assert response.status_code == 200
    assert response.json()["version"] == "1.0.0"

    response = client.get("/api/v1/asset-db/versions/api-pack")
    assert response.status_code == 200
    assert response.json()["count"] == 2
    assert [item["version"] for item in response.json()["versions"]] == [
        "1.0.0",
        "0.9.0",
    ]

    response = client.get("/api/v1/asset-db/categories")
    assert response.status_code == 200
    assert response.json()["types"]["widget_pack"] == 1

    response = client.get("/api/v1/asset-db/stats")
    assert response.status_code == 200
    assert response.json()["packs"] == 1
    assert response.json()["policy_mode"] == "offline"


def test_router_install_from_cache_and_typed_conflicts(tmp_path):
    repository = _repo(tmp_path)
    files, manifest = _pack("install-pack")
    repository.index.add(AssetPackEntry.from_manifest(manifest))
    repository.install(_write_pack(tmp_path / "install-src", files))
    client = _client(repository)

    response = client.post("/api/v1/asset-db/install/install-pack")
    assert response.status_code == 200, response.text
    body = response.json()
    assert body["ok"] is True
    assert body["version"] == "1.0.0"
    assert (tmp_path / "widget_packs" / "install-pack" / "manifest.json").is_file()

    _, yanked = _pack("yanked-api", yanked=True)
    repository.index.add(AssetPackEntry.from_manifest(yanked))
    response = client.post(
        "/api/v1/asset-db/install/yanked-api", params={"auto": "true"}
    )
    assert response.status_code == 409
    assert response.json()["detail"]["code"] == "asset_conflict"


def test_router_typed_400_404_and_submit_paths(tmp_path):
    repository = _repo(tmp_path)
    _, manifest = _pack("submit-pack")
    repository.index.add(AssetPackEntry.from_manifest(manifest))
    _, disallowed = _pack("gpl-pack", license="GPL-2.0-only")
    repository.index.add(AssetPackEntry.from_manifest(disallowed))
    client = _client(repository)

    response = client.get("/api/v1/asset-db/search", params={"type": "bogus"})
    assert response.status_code == 400
    assert response.json()["detail"]["code"] == "invalid_asset"

    response = client.get("/api/v1/asset-db/entry/missing")
    assert response.status_code == 404
    assert response.json()["detail"]["code"] == "asset_not_found"

    response = client.get("/api/v1/asset-db/versions/missing")
    assert response.status_code == 404

    response = client.post("/api/v1/asset-db/install/missing")
    assert response.status_code == 404

    response = client.post("/api/v1/asset-db/submit/submit-pack")
    assert response.status_code == 200
    assert response.json()["status"] == "queued"
    assert repository.cache.submissions_path.is_file()

    response = client.post("/api/v1/asset-db/submit/gpl-pack")
    assert response.status_code == 400
    assert response.json()["detail"]["code"] == "invalid_asset"

    _, import_manifest = _pack("private-import", type="import_pack")
    repository.index.add(AssetPackEntry.from_manifest(import_manifest))
    response = client.post("/api/v1/asset-db/submit/private-import")
    assert response.status_code == 409
    response = client.post(
        "/api/v1/asset-db/submit/private-import",
        json={"attestation": True},
    )
    assert response.status_code == 200


def test_router_sync_offline(tmp_path):
    repository = _repo(tmp_path, policy=AssetSyncPolicy(mode="offline"))
    client = _client(repository)
    response = client.post("/api/v1/asset-db/sync")
    assert response.status_code == 200
    assert response.json()["offline"] is True
    assert response.json()["synced"] is False


def test_asset_cache_rejects_unsafe_ids_and_versions(tmp_path):
    cache = AssetCache(tmp_path / "cache")
    with pytest.raises(InvalidAssetError):
        cache.pack_dir("../escape", "1.0.0")
    with pytest.raises(UnsafeArchiveError):
        cache.pack_dir("safe-pack", "../1.0.0")
