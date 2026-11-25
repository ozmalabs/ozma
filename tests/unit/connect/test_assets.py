"""Tests for the Connect shared asset database (M2 / WGT-030..WGT-036)."""

from __future__ import annotations

import copy
import gzip
import sys
from pathlib import Path

import pytest

# When pytest collects this directory it may put ``tests/unit/connect`` on
# sys.path, shadowing the repository's namespace package.  Prefer the repo root.
_ORIGINAL_SYS_PATH = list(sys.path)
_REPO_ROOT = Path(__file__).resolve().parents[3]
if str(_REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(_REPO_ROOT))
_SHADOW = Path(__file__).resolve().parents[0]
sys.path[:] = [
    entry
    for entry in sys.path
    if entry
    and Path(entry).resolve() != _SHADOW
    and not (Path(entry).resolve() / "connect.py").exists()
]
for _name in list(sys.modules):
    if _name == "connect" or _name.startswith("connect."):
        sys.modules.pop(_name, None)

from fastapi import FastAPI  # noqa: E402
from fastapi.testclient import TestClient  # noqa: E402
from sqlalchemy import event, inspect  # noqa: E402
from sqlalchemy.orm import Session  # noqa: E402

from connect.assets.index import (  # noqa: E402
    HAVE_ED25519,
    AssetService,
    Ed25519Signer,
    Ed25519Verifier,
    HMACSigner,
    HMACVerifier,
    build_index,
    index_bytes,
    verify_index,
)
from connect.assets.models import (  # noqa: E402
    ASSET_SCHEMA,
    AssetFile,
    AssetNotFoundError,
    AssetPack,
    AssetVersion,
    InvalidAssetError,
    StorageLimitError,
    StorageNotFoundError,
    UnsafeStorageKey,
    create_schema,
    create_sqlite_engine,
    json_dumps,
    license_is_allowed,
)
from connect.assets.router import build_router  # noqa: E402
from connect.assets.storage import (  # noqa: E402
    FileStore,
    MemoryStore,
    content_key,
)

# Do not leak the import workaround into tests collected later in the session.
sys.path[:] = _ORIGINAL_SYS_PATH

pytestmark = pytest.mark.unit

_INDEX_LIMIT = 2 * 1024 * 1024


@pytest.fixture
def engine(tmp_path):
    database = tmp_path / "assets.db"
    engine = create_sqlite_engine(database)

    @event.listens_for(engine, "connect")
    def _enable_foreign_keys(dbapi_connection, _record):
        cursor = dbapi_connection.cursor()
        cursor.execute("PRAGMA foreign_keys=ON")
        cursor.close()

    create_schema(engine)
    yield engine
    engine.dispose()


@pytest.fixture
def store():
    return MemoryStore()


@pytest.fixture
def signer():
    return HMACSigner(b"unit-test-signing-key", key_id="unit")


@pytest.fixture
def verifier():
    return HMACVerifier(b"unit-test-signing-key")


@pytest.fixture
def service(engine, store, signer, verifier):
    return AssetService(
        engine,
        store=store,
        signer=signer,
        verifier=verifier,
    )


def _manifest(
    pack_id: str = "org.ozma.widgets.retro-gauge",
    version: str = "1.0.0",
    **overrides,
) -> dict:
    manifest = {
        "id": pack_id,
        "type": "widget_pack",
        "name": "Retro Gauge",
        "version": version,
        "schema": ASSET_SCHEMA,
        "author": "Ozma",
        "description": "A retro gauge with an amber palette",
        "license": "MIT",
        "source_url": "https://example.invalid/source",
        "tags": ["gauge", "audio"],
        "capabilities": ["rendered_widget"],
        "permissions": ["ha_read"],
        "assets": [
            {
                "path": "assets/gauge.svg",
                "sha256": "a" * 64,
                "size": 128,
                "mime": "image/svg+xml",
                "executable": False,
            }
        ],
        "provenance": {
            "type": "original",
            "author": "Ozma",
            "source": "fixture",
        },
        "vibe": {"mood": ["retro"], "palette": "amber"},
        "shape": {"family": "radial", "geometry": "circular"},
        "coherence": {"score": 0.9},
        "published_at": "2026-01-01T00:00:00+00:00",
        "yanked": False,
    }
    manifest.update(overrides)
    return manifest


def _submit_public(service: AssetService, manifest: dict) -> dict:
    return service.submit(
        manifest,
        attestation={"redistributable": True},
        private=False,
    )


def _client(service: AssetService) -> TestClient:
    app = FastAPI()
    app.include_router(build_router(service))
    return TestClient(app)


# ── Schema ───────────────────────────────────────────────────────────────────


def test_schema_creation_is_idempotent_and_indexed(engine):
    create_schema(engine)  # The migration must be safe to re-apply.

    inspector = inspect(engine)
    tables = set(inspector.get_table_names())
    assert {
        "asset_pack",
        "asset_version",
        "asset_file",
        "asset_tag",
        "asset_provenance",
        "asset_report",
    } <= tables

    pack_indexes = {
        index["name"] for index in inspector.get_indexes("asset_pack")
    }
    assert {
        "ix_asset_pack_type",
        "ix_asset_pack_license",
        "ix_asset_pack_yanked",
    } <= pack_indexes
    tag_indexes = {
        index["name"] for index in inspector.get_indexes("asset_tag")
    }
    assert "ix_asset_tag_tag" in tag_indexes

    version_uniques = inspector.get_unique_constraints("asset_version")
    assert any(
        set(item["column_names"]) == {"pack_id", "version"}
        for item in version_uniques
    )

    file_foreign_keys = inspector.get_foreign_keys("asset_file")
    assert any(
        set(item["constrained_columns"]) == {"pack_id", "version"}
        for item in file_foreign_keys
    )


# ── Submission, search, and metadata ────────────────────────────────────────


def test_submit_search_entry_versions_categories_and_stats(service):
    gauge = _manifest()
    theme = _manifest(
        pack_id="org.ozma.themes.amber",
        version="2.1.0",
        type="theme_pack",
        name="Amber Theme",
        tags=["theme", "noc"],
        permissions=["internet"],
    )
    _submit_public(service, gauge)
    _submit_public(service, theme)

    search = service.search("gauge", type="widget_pack")
    assert search["total"] == 1
    assert search["entries"][0]["id"] == gauge["id"]

    assert service.search("retro")["total"] == 2  # vibe text
    assert service.search("radial")["total"] == 2  # shape text
    assert service.search("amber")["total"] == 2
    assert service.search(type="theme_pack")["total"] == 1
    assert service.search(tag="audio")["total"] == 1
    assert service.search(licence="MIT")["total"] == 2
    assert service.search(permission="internet")["total"] == 1
    assert service.search(q="gauge", limit=0)["count"] == 0
    page = service.search(limit=1, offset=1)
    assert page["count"] == 1
    assert page["total"] == 2

    entry = service.entry(gauge["id"])
    assert entry["id"] == gauge["id"]
    assert entry["version"] == "1.0.0"
    assert entry["assets"][0]["path"] == "assets/gauge.svg"

    versions = service.versions(gauge["id"])
    assert versions["count"] == 1
    assert versions["versions"][0]["version"] == "1.0.0"

    categories = service.categories()
    assert categories["types"] == {"theme_pack": 1, "widget_pack": 1}
    assert categories["tags"]["gauge"] == 1
    assert categories["permissions"]["internet"] == 1
    assert categories["licences"]["MIT"] == 2

    stats = service.stats()
    assert stats["packs"] == 2
    assert stats["versions"] == 2
    assert stats["total_packs"] == 2
    assert stats["by_type"] == {"theme_pack": 1, "widget_pack": 1}
    assert stats["total_bytes"] == 256


def test_unknown_manifest_fields_are_preserved_losslessly(service):
    manifest = _manifest()
    manifest["custom_top"] = {"keep": [1, 2, 3]}
    manifest["vibe"]["custom_vibe"] = "sunset"
    manifest["assets"][0]["custom_asset"] = "kept"
    manifest["provenance"]["custom_provenance"] = "kept"
    _submit_public(service, manifest)

    entry = service.entry(manifest["id"])
    assert entry["custom_top"] == {"keep": [1, 2, 3]}
    assert entry["vibe"]["custom_vibe"] == "sunset"
    assert entry["assets"][0]["custom_asset"] == "kept"
    assert entry["provenance"]["custom_provenance"] == "kept"
    assert entry["id"] == manifest["id"]


def test_private_is_the_default_and_absent_from_public_views(service):
    manifest = _manifest(pack_id="org.ozma.widgets.private")
    result = service.submit(manifest)
    assert result["private"] is True
    assert result["status"] == "private"

    assert service.search("gauge")["total"] == 0
    assert service.stats()["packs"] == 0
    assert service.stats()["private_packs"] == 1
    with pytest.raises(AssetNotFoundError):
        service.entry(manifest["id"])


# ── Licensing and submission gates ──────────────────────────────────────────


@pytest.mark.parametrize(
    "licence",
    [
        "MIT",
        "BSD-2-Clause",
        "BSD-3-Clause",
        "ISC",
        "Apache-2.0",
        "OFL-1.1",
        "CC0-1.0",
    ],
)
def test_public_allowlisted_licences_are_accepted(service, licence):
    manifest = _manifest(
        pack_id=f"org.ozma.licence.{licence.lower().replace('.', '-')}",
        license=licence,
    )
    result = _submit_public(service, manifest)
    assert result["license"] == licence


def test_public_non_allowlisted_licence_is_rejected(service):
    with pytest.raises(InvalidAssetError, match="allowlist"):
        _submit_public(
            service,
            _manifest(pack_id="org.ozma.widgets.gpl", license="GPL-2.0-only"),
        )


def test_agpl_requires_first_party(service):
    with pytest.raises(InvalidAssetError):
        _submit_public(
            service,
            _manifest(pack_id="org.ozma.widgets.agpl", license="AGPL-3.0-only"),
        )

    manifest = _manifest(
        pack_id="org.ozma.widgets.agpl-first-party",
        license="AGPL-3.0-only",
    )
    manifest["provenance"]["first_party"] = True
    result = _submit_public(service, manifest)
    assert result["status"] == "pending_review"


def test_licence_expression_helpers():
    assert license_is_allowed("MIT")
    assert license_is_allowed("MIT OR Apache-2.0")
    assert not license_is_allowed("MIT AND GPL-2.0-only")
    assert not license_is_allowed("AGPL-3.0-only")
    assert license_is_allowed("AGPL-3.0-only", first_party=True)


def test_provenance_is_required(service):
    manifest = _manifest(pack_id="org.ozma.widgets.no-provenance")
    manifest.pop("provenance")
    with pytest.raises(InvalidAssetError, match="provenance"):
        service.submit(manifest)


def test_executable_assets_require_a_sandbox(service):
    manifest = _manifest(pack_id="org.ozma.widgets.exec")
    manifest["assets"][0]["executable"] = True
    manifest["assets"][0].pop("sandbox", None)
    with pytest.raises(InvalidAssetError, match="sandbox"):
        service.submit(manifest)

    manifest["assets"][0]["sandbox"] = "iframe"
    result = service.submit(manifest)
    assert result["ok"] is True


def test_public_submission_requires_attestation(service):
    with pytest.raises(InvalidAssetError, match="attestation"):
        service.submit(_manifest(), private=False)


def test_private_submission_can_use_non_allowlisted_licence(service):
    manifest = _manifest(
        pack_id="org.ozma.widgets.private-import",
        type="import_pack",
        license="GPL-2.0-only",
    )
    result = service.submit(manifest)
    assert result["private"] is True


# ── Yank + sync snapshot ────────────────────────────────────────────────────


def test_yanked_versions_are_excluded_by_default(service):
    _submit_public(service, _manifest(version="1.0.0", name="Retro Gauge"))
    _submit_public(
        service,
        _manifest(version="2.0.0", name="Retro Gauge", yanked=True),
    )

    default = service.search("gauge")
    assert default["total"] == 1
    assert default["entries"][0]["version"] == "1.0.0"

    included = service.search("gauge", include_yanked=True)
    assert included["total"] == 1
    assert included["entries"][0]["version"] == "2.0.0"

    versions = service.versions("org.ozma.widgets.retro-gauge")
    assert versions["count"] == 2
    trimmed = service.versions(
        "org.ozma.widgets.retro-gauge",
        include_yanked=False,
    )
    assert trimmed["count"] == 1
    assert service.entry("org.ozma.widgets.retro-gauge")["version"] == "1.0.0"

    stable = build_index(
        service.engine,
        channel="stable",
        generated_at="2026-01-01T00:00:00+00:00",
    )
    assert [entry["version"] for entry in stable["entries"]] == ["1.0.0"]
    all_versions = build_index(
        service.engine,
        channel="all",
        generated_at="2026-01-01T00:00:00+00:00",
    )
    assert [entry["version"] for entry in all_versions["entries"]] == [
        "1.0.0",
        "2.0.0",
    ]


def test_snapshot_etag_and_if_none_match(service, verifier):
    _submit_public(service, _manifest())

    snapshot = service.snapshot("stable")
    assert snapshot.status_code == 200
    assert snapshot.not_modified is False
    assert snapshot["etag"] == snapshot.etag
    assert snapshot.index is not None
    assert verify_index(snapshot.index, verifier)
    assert snapshot.headers["ETag"] == snapshot.etag

    cached = service.snapshot("stable", if_none_match=snapshot.etag)
    assert cached.not_modified is True
    assert cached.status_code == 304
    assert cached.index is None

    weak = service.snapshot("stable", if_none_match=f"W/{snapshot.etag}")
    assert weak.not_modified is True
    star = service.snapshot("stable", if_none_match="*")
    assert star.not_modified is True
    changed = service.snapshot("stable", if_none_match='"different"')
    assert changed.not_modified is False


# ── Storage ─────────────────────────────────────────────────────────────────


def test_memory_store_is_content_addressed_and_rejects_unsafe_keys():
    store = MemoryStore()
    payload = b"gauge-bytes"
    key = store.put_content(payload)
    assert key == content_key(payload)
    assert key.startswith("sha256/")
    assert store.exists(key)
    assert store.get(key) == payload
    assert store.put(b"auto-addressed") == content_key(b"auto-addressed")
    assert store.signed_url(key).startswith("memory://")
    assert store.delete(key) is True
    assert store.exists(key) is False
    with pytest.raises(StorageNotFoundError):
        store.get(key)

    for unsafe in ("../escape", "/absolute", "assets\\file", "a/../b", ""):
        with pytest.raises(UnsafeStorageKey):
            store.put(unsafe, b"x")

    tiny = MemoryStore(max_bytes=4)
    with pytest.raises(StorageLimitError):
        tiny.put("sha256/aa/big", b"12345")


def test_file_store_round_trip_and_missing_object(tmp_path):
    store = FileStore(tmp_path / "objects", max_bytes=16)
    key = store.put_content(b"hello")
    assert store.exists(key)
    assert store.get(key) == b"hello"
    assert store.signed_url(key).startswith("file://")
    assert store.delete(key) is True
    assert store.delete(key) is False
    with pytest.raises(StorageNotFoundError):
        store.get(key)
    with pytest.raises(UnsafeStorageKey):
        store.put("../outside", b"x")


# ── Deterministic index and signatures ──────────────────────────────────────


def test_index_is_deterministic_and_tamper_evident(service, verifier):
    _submit_public(service, _manifest())
    generated_at = "2026-02-03T04:05:06+00:00"
    first = build_index(
        service.engine,
        channel="stable",
        generated_at=generated_at,
        signer=service.signer,
    )
    second = build_index(
        service.engine,
        channel="stable",
        generated_at=generated_at,
        signer=service.signer,
    )
    assert index_bytes(first) == index_bytes(second)
    assert first["entries"]
    assert verify_index(first, verifier)

    tampered_entry = copy.deepcopy(first)
    tampered_entry["entries"][0]["name"] = "Evil Gauge"
    assert verify_index(tampered_entry, verifier) is False

    tampered_signature = copy.deepcopy(first)
    tampered_signature["signature"]["value"] = "00" * 32
    assert verify_index(tampered_signature, verifier) is False

    assert verify_index(first, HMACVerifier(b"wrong-key")) is False
    assert verify_index(
        first,
        lambda payload, signature: verifier.verify(payload, signature),
    )
    assert verify_index(first, None) is False


@pytest.mark.skipif(not HAVE_ED25519, reason="cryptography is not installed")
def test_ed25519_signature_round_trip(service):
    from cryptography.hazmat.primitives.asymmetric.ed25519 import (
        Ed25519PrivateKey,
    )

    private_key = Ed25519PrivateKey.generate()
    signer = Ed25519Signer(private_key, key_id="ed25519-test")
    verifier = Ed25519Verifier(private_key.public_key())
    _submit_public(service, _manifest())
    index = build_index(
        service.engine,
        channel="stable",
        generated_at="2026-01-01T00:00:00+00:00",
        signer=signer,
    )
    assert index["signature"]["algorithm"] == "ed25519"
    assert verify_index(index, verifier)


def test_index_stays_within_the_performance_budget(engine):
    with Session(engine) as session:
        for number in range(250):
            pack_id = f"org.ozma.widgets.pack-{number:04d}"
            manifest = _manifest(pack_id=pack_id)
            pack = AssetPack(
                id=pack_id,
                type="widget_pack",
                name=f"Pack {number}",
                description="Small fixture pack",
                license="MIT",
                author="Ozma",
                published_at="2026-01-01T00:00:00+00:00",
                private=False,
            )
            version = AssetVersion(
                pack_id=pack_id,
                version="1.0.0",
                manifest_json=json_dumps(manifest),
                published_at="2026-01-01T00:00:00+00:00",
            )
            version.files = [
                AssetFile(
                    pack_id=pack_id,
                    version="1.0.0",
                    path="assets/gauge.svg",
                    sha256="a" * 64,
                    size_bytes=128,
                    mime_type="image/svg+xml",
                )
            ]
            pack.versions = [version]
            session.add(pack)
        session.commit()

    index = build_index(
        engine,
        channel="stable",
        generated_at="2026-01-01T00:00:00+00:00",
    )
    assert len(index["entries"]) == 250
    body = index_bytes(index)
    assert len(body) < 512 * 1024  # smaller bound for the 250-entry fixture
    # The documented budget is 2 MiB gzip for 10k packs; project the actual
    # compressed ratio rather than requiring uncompressed JSON to hit it.
    compressed = gzip.compress(body)
    assert len(compressed) / 250 * 10_000 <= _INDEX_LIMIT


# ── Router ──────────────────────────────────────────────────────────────────


def test_router_success_and_typed_errors(service):
    manifest = _manifest()
    submission = {
        "manifest": manifest,
        "attestation": {"ok": True},
        "private": False,
    }
    with _client(service) as client:
        submitted = client.post("/assets/submit", json=submission)
        assert submitted.status_code == 200
        assert submitted.json()["status"] == "pending_review"

        search = client.get("/assets/search", params={"q": "gauge"})
        assert search.status_code == 200
        assert search.json()["total"] == 1

        entry = client.get(f"/assets/entry/{manifest['id']}")
        assert entry.status_code == 200
        assert entry.json()["id"] == manifest["id"]

        versions = client.get(f"/assets/versions/{manifest['id']}")
        assert versions.status_code == 200
        assert versions.json()["count"] == 1

        categories = client.get("/assets/categories")
        assert categories.status_code == 200
        assert categories.json()["types"]["widget_pack"] == 1

        stats = client.get("/assets/stats")
        assert stats.status_code == 200
        assert stats.json()["packs"] == 1

        missing = client.get("/assets/entry/org.ozma.nope")
        assert missing.status_code == 404
        assert missing.json()["detail"]["code"] == "not_found"

        duplicate = client.post("/assets/submit", json=submission)
        assert duplicate.status_code == 409
        assert duplicate.json()["detail"]["code"] == "conflict"

        bad_licence = _manifest(
            pack_id="org.ozma.widgets.bad-licence",
            license="GPL-2.0-only",
        )
        rejected = client.post(
            "/assets/submit",
            json={
                "manifest": bad_licence,
                "attestation": True,
                "private": False,
            },
        )
        assert rejected.status_code == 400
        assert rejected.json()["detail"]["code"] == "invalid_asset"

        malformed = client.post("/assets/submit", json={"version": "1.0.0"})
        assert malformed.status_code == 400
        not_an_object = client.post("/assets/submit", json=[manifest])
        assert not_an_object.status_code == 400

        index = client.get("/assets/index/stable")
        assert index.status_code == 200
        etag = index.headers["etag"]
        assert index.json()["etag"] == etag
        assert index.headers["x-content-sha256"]

        not_modified = client.get(
            "/assets/index/stable",
            headers={"If-None-Match": etag},
        )
        assert not_modified.status_code == 304
        assert not_modified.content == b""


def test_router_accepts_a_bare_manifest_body(service):
    manifest = _manifest(pack_id="org.ozma.widgets.bare")
    manifest["private"] = False
    manifest["attestation"] = {"redistributable": True}
    with _client(service) as client:
        response = client.post("/assets/submit", json=manifest)
        assert response.status_code == 200
        assert response.json()["private"] is False
        assert client.get("/assets/search").json()["total"] == 1
