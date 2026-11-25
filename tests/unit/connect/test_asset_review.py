"""Tests for the optional Connect review workflow and asset gallery."""

from __future__ import annotations

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
from sqlalchemy import event, inspect, select  # noqa: E402
from sqlalchemy.orm import Session  # noqa: E402

from connect.assets.bootstrap import configure_assets  # noqa: E402
from connect.assets.gallery import CSP  # noqa: E402
from connect.assets.index import AssetService, build_index  # noqa: E402
from connect.assets.models import (  # noqa: E402
    ASSET_SCHEMA,
    AssetNotFoundError,
    AssetReview,
    AssetReviewEvent,
    InvalidAssetError,
    create_schema,
    create_sqlite_engine,
)
from connect.assets.router import build_router  # noqa: E402
from connect.assets.storage import MemoryStore  # noqa: E402

# Do not leak the import workaround into tests collected later in the session.
sys.path[:] = _ORIGINAL_SYS_PATH

pytestmark = pytest.mark.unit


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


def _manifest(
    pack_id: str = "org.ozma.widgets.review",
    version: str = "1.0.0",
    **overrides,
) -> dict:
    manifest = {
        "id": pack_id,
        "type": "widget_pack",
        "name": "Review Gauge",
        "version": version,
        "schema": ASSET_SCHEMA,
        "author": "Ozma",
        "description": "A review workflow fixture",
        "license": "MIT",
        "source_url": "https://example.invalid/source",
        "tags": ["gauge", "review"],
        "capabilities": ["rendered_widget"],
        "permissions": ["ha_read"],
        "assets": [
            {
                "path": "assets/gauge.svg",
                "sha256": "d" * 64,
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
        "published_at": "2026-01-01T00:00:00+00:00",
        "yanked": False,
    }
    manifest.update(overrides)
    return manifest


def _service(engine, *, review_required: bool = False, token: str | None = None):
    return AssetService(
        engine,
        store=MemoryStore(),
        review_required=review_required,
        review_token=token,
    )


def _submit_public(service: AssetService, manifest: dict) -> dict:
    return service.submit(
        manifest,
        attestation={"redistributable": True},
        private=False,
    )


def _client(service: AssetService, **kwargs) -> TestClient:
    app = FastAPI()
    app.include_router(build_router(service, **kwargs))
    return TestClient(app)


def _review_rows(engine):
    with Session(engine) as session:
        return session.execute(select(AssetReview)).scalars().all()


def _review_events(engine):
    with Session(engine) as session:
        return session.execute(select(AssetReviewEvent)).scalars().all()


# ── Schema and backward compatibility ───────────────────────────────────────


def test_review_schema_and_migration_are_idempotent(engine) -> None:
    create_schema(engine)  # Re-apply both checked-in migrations.

    inspector = inspect(engine)
    tables = set(inspector.get_table_names())
    assert {"asset_review", "asset_review_event"} <= tables

    review_indexes = {
        item["name"] for item in inspector.get_indexes("asset_review")
    }
    assert "ix_asset_review_status" in review_indexes

    event_indexes = {
        item["name"] for item in inspector.get_indexes("asset_review_event")
    }
    assert {
        "ix_asset_review_event_pack_id",
        "ix_asset_review_event_status",
    } <= event_indexes

    review_primary = inspector.get_pk_constraint("asset_review")
    assert set(review_primary["constrained_columns"]) == {"pack_id", "version"}


def test_review_off_keeps_public_submissions_visible(engine) -> None:
    service = _service(engine)
    manifest = _manifest()
    result = _submit_public(service, manifest)

    assert result["review_status"] == "approved"
    assert result["review_required"] is False
    assert service.search("gauge")["total"] == 1
    assert service.stats()["packs"] == 1
    assert build_index(
        engine,
        channel="stable",
        generated_at="2026-01-01T00:00:00+00:00",
    )["entries"]
    assert [
        row.status for row in _review_rows(engine)
    ] == ["approved"]

    with _client(service) as client:
        gallery = client.get("/assets/gallery")
    assert manifest["id"] in gallery.text


def test_missing_review_row_defaults_to_approved(engine) -> None:
    service = _service(engine)
    manifest = _manifest()
    _submit_public(service, manifest)

    # Simulate a public pack created before review support existed.
    with Session(engine) as session:
        for row in session.execute(select(AssetReview)).scalars():
            session.delete(row)
        session.commit()

    reviewed = _service(engine, review_required=True)
    assert reviewed.search("gauge")["total"] == 1
    assert reviewed.entry(manifest["id"])["id"] == manifest["id"]
    assert reviewed.snapshot("stable").index["entries"]


# ── Pending / approved / rejected state ─────────────────────────────────────


def test_pending_public_submission_is_excluded_when_review_required(engine) -> None:
    service = _service(engine, review_required=True)
    manifest = _manifest(name="Pending Gauge")
    result = _submit_public(service, manifest)

    assert result["review_status"] == "pending"
    assert [row.status for row in _review_rows(engine)] == ["pending"]
    assert service.search("gauge")["total"] == 0
    assert service.stats()["packs"] == 0
    assert service.categories()["types"] == {}
    assert service.snapshot("stable").index["entries"] == []
    assert build_index(
        engine,
        channel="stable",
        generated_at="2026-01-01T00:00:00+00:00",
        review_required=True,
    )["entries"] == []
    with pytest.raises(AssetNotFoundError):
        service.entry(manifest["id"])
    with pytest.raises(AssetNotFoundError):
        service.versions(manifest["id"])

    pending = service.reviews.list_reviews("pending")
    assert pending["total"] == 1
    assert pending["reviews"][0]["id"] == manifest["id"]

    with _client(service, review_token=None) as client:
        # No token configured: review listing is closed, but the gallery is public.
        assert client.get("/assets/review").status_code == 503
        gallery = client.get("/assets/gallery")
    assert "Pending Gauge" not in gallery.text


def test_private_and_first_party_submissions_default_to_approved(engine) -> None:
    service = _service(engine, review_required=True)

    private_manifest = _manifest(pack_id="org.ozma.widgets.private-review")
    private_result = service.submit(private_manifest)
    assert private_result["private"] is True
    assert private_result["review_status"] == "approved"

    first_party = _manifest(pack_id="org.ozma.widgets.first-party-review")
    first_party["provenance"]["first_party"] = True
    first_party_result = _submit_public(service, first_party)
    assert first_party_result["review_status"] == "approved"

    statuses = {
        row.pack_id: row.status for row in _review_rows(engine)
    }
    assert statuses[private_manifest["id"]] == "approved"
    assert statuses[first_party["id"]] == "approved"
    assert service.search("gauge")["total"] == 1
    assert service.stats()["packs"] == 1
    assert service.stats()["private_packs"] == 1


def test_approve_transitions_and_writes_audit(engine) -> None:
    service = _service(engine, review_required=True)
    manifest = _manifest(name="Approval Gauge")
    _submit_public(service, manifest)

    decision = service.reviews.approve(
        manifest["id"],
        note="looks good",
        reviewer="alice",
    )
    assert decision["status"] == "approved"
    assert decision["unchanged"] is False
    assert decision["reviewed"] is True

    rows = _review_rows(engine)
    assert len(rows) == 1
    assert rows[0].status == "approved"
    assert rows[0].reviewer == "alice"
    assert rows[0].note == "looks good"
    assert rows[0].reviewed_at

    events = _review_events(engine)
    assert len(events) == 1
    assert events[0].status == "approved"
    assert events[0].previous_status == "pending"
    assert events[0].reviewer == "alice"
    assert events[0].note == "looks good"

    assert service.search("gauge")["total"] == 1
    assert service.entry(manifest["id"])["name"] == "Approval Gauge"
    assert service.snapshot("stable").index["entries"][0]["id"] == manifest["id"]
    assert service.stats()["packs"] == 1


def test_reject_requires_note_and_writes_audit(engine) -> None:
    service = _service(engine, review_required=True)
    manifest = _manifest()
    _submit_public(service, manifest)

    with pytest.raises(InvalidAssetError) as caught:
        service.reviews.reject(manifest["id"])
    assert caught.value.code == "review_note_required"

    decision = service.reviews.reject(
        manifest["id"],
        note="licence evidence missing",
        reviewer="bob",
    )
    assert decision["status"] == "rejected"
    assert decision["unchanged"] is False

    assert service.search("gauge")["total"] == 0
    assert service.snapshot("stable").index["entries"] == []
    events = _review_events(engine)
    assert len(events) == 1
    assert events[0].status == "rejected"
    assert events[0].previous_status == "pending"
    assert events[0].note == "licence evidence missing"


def test_duplicate_approve_is_idempotent(engine) -> None:
    service = _service(engine, review_required=True)
    manifest = _manifest()
    _submit_public(service, manifest)

    first = service.reviews.approve(manifest["id"], note="ok", reviewer="alice")
    second = service.reviews.approve(
        manifest["id"],
        note="ok again",
        reviewer="alice",
    )

    assert first["unchanged"] is False
    assert second["unchanged"] is True
    assert second["status"] == "approved"
    assert len(_review_events(engine)) == 1
    assert len(_review_rows(engine)) == 1


def test_duplicate_reject_is_idempotent(engine) -> None:
    service = _service(engine, review_required=True)
    manifest = _manifest()
    _submit_public(service, manifest)

    service.reviews.reject(manifest["id"], note="bad", reviewer="bob")
    second = service.reviews.reject(manifest["id"], note="bad", reviewer="bob")
    assert second["unchanged"] is True
    assert len(_review_events(engine)) == 1


def test_approval_of_new_version_refreshes_public_summary(engine) -> None:
    service = _service(engine, review_required=True)
    first = _manifest(version="1.0.0", name="Old Gauge", tags=["gauge", "old"])
    _submit_public(service, first)
    service.reviews.approve(first["id"], note="initial")

    second = _manifest(
        version="2.0.0",
        name="New Gauge",
        tags=["gauge", "new"],
        description="Newer metadata that is still pending",
    )
    _submit_public(service, second)

    entry = service.entry(second["id"])
    assert entry["version"] == "1.0.0"
    assert entry["name"] == "Old Gauge"
    assert "new" not in service.search("new")["entries"]

    service.reviews.approve(second["id"], note="new version")
    entry = service.entry(second["id"])
    assert entry["version"] == "2.0.0"
    assert entry["name"] == "New Gauge"
    assert service.search("new")["total"] == 1


def test_rejecting_newest_version_falls_back_to_approved_metadata(engine) -> None:
    service = _service(engine, review_required=True)
    first = _manifest(version="1.0.0", name="Stable Gauge")
    _submit_public(service, first)
    service.reviews.approve(first["id"], note="initial")

    second = _manifest(version="2.0.0", name="Temporary Gauge")
    _submit_public(service, second)
    service.reviews.approve(second["id"], note="new version")
    assert service.entry(second["id"])["version"] == "2.0.0"

    service.reviews.reject(second["id"], note="withdrawn")
    entry = service.entry(second["id"])
    assert entry["version"] == "1.0.0"
    assert entry["name"] == "Stable Gauge"
    assert service.search("temporary")["total"] == 0


# ── HTTP review API ─────────────────────────────────────────────────────────


def test_review_endpoints_require_token(engine) -> None:
    service = _service(engine, review_required=True, token="s3cret")
    manifest = _manifest()
    _submit_public(service, manifest)

    with _client(service) as client:
        missing = client.get("/assets/review")
        invalid = client.get(
            "/assets/review",
            headers={"X-Ozma-Review-Token": "wrong"},
        )
        allowed = client.get(
            "/assets/review",
            headers={"X-Ozma-Review-Token": "s3cret"},
        )

    assert missing.status_code == 401
    assert missing.json()["detail"]["code"] == "review_token_missing"
    assert invalid.status_code == 403
    assert invalid.json()["detail"]["code"] == "review_token_invalid"
    assert allowed.status_code == 200
    assert allowed.json()["total"] == 1

    unconfigured = _service(engine, review_required=True, token=None)
    with _client(unconfigured) as client:
        closed = client.get("/assets/review")
        closed_post = client.post(
            f"/assets/review/{manifest['id']}/approve",
            json={},
        )
    assert closed.status_code == 503
    assert closed.json()["detail"]["code"] == "review_unavailable"
    assert closed_post.status_code == 503


def test_review_endpoint_transitions_and_typed_errors(engine) -> None:
    service = _service(engine, review_required=True, token="s3cret")
    headers = {"X-Ozma-Review-Token": "s3cret"}
    manifest = _manifest()
    _submit_public(service, manifest)

    with _client(service) as client:
        missing_note = client.post(
            f"/assets/review/{manifest['id']}/reject",
            json={},
            headers=headers,
        )
        assert missing_note.status_code == 400
        assert missing_note.json()["detail"]["code"] == "review_note_required"

        bad_status = client.get(
            "/assets/review",
            params={"status": "bogus"},
            headers=headers,
        )
        assert bad_status.status_code == 400
        assert bad_status.json()["detail"]["code"] == "invalid_review_status"

        unknown_pack = client.post(
            "/assets/review/org.ozma.nope/approve",
            json={},
            headers=headers,
        )
        assert unknown_pack.status_code == 404

        unknown_version = client.post(
            f"/assets/review/{manifest['id']}/approve",
            json={"version": "9.9.9"},
            headers=headers,
        )
        assert unknown_version.status_code == 404

        non_object = client.post(
            f"/assets/review/{manifest['id']}/approve",
            json=["not", "an", "object"],
            headers=headers,
        )
        assert non_object.status_code == 400

        approved = client.post(
            f"/assets/review/{manifest['id']}/approve",
            json={"note": "ok"},
            headers={**headers, "X-Ozma-Review-Reviewer": "carol"},
        )
        assert approved.status_code == 200
        assert approved.json()["status"] == "approved"
        assert approved.json()["reviewer"] == "carol"

        repeated = client.post(
            f"/assets/review/{manifest['id']}/approve",
            json={"note": "again"},
            headers=headers,
        )
        assert repeated.status_code == 200
        assert repeated.json()["unchanged"] is True

        approved_list = client.get(
            "/assets/review",
            params={"status": "approved"},
            headers=headers,
        )
        assert approved_list.json()["total"] == 1

    assert len(_review_events(engine)) == 1


def test_review_rejects_private_and_yanked_versions(engine) -> None:
    service = _service(engine, review_required=True, token="s3cret")
    headers = {"X-Ozma-Review-Token": "s3cret"}

    private_manifest = _manifest(pack_id="org.ozma.widgets.private-conflict")
    service.submit(private_manifest)

    with _client(service) as client:
        private = client.post(
            f"/assets/review/{private_manifest['id']}/approve",
            json={},
            headers=headers,
        )
    assert private.status_code == 409
    assert private.json()["detail"]["code"] == "review_conflict"


# ── Gallery ─────────────────────────────────────────────────────────────────


def test_gallery_is_self_contained_escaped_and_excludes_non_public(engine) -> None:
    service = _service(engine, review_required=True)

    pending = _manifest(pack_id="org.ozma.widgets.pending-gallery")
    _submit_public(service, pending)

    malicious = _manifest(
        pack_id="org.ozma.widgets.approved-gallery",
        name='<script>alert("x")</script> Gauge',
        author='<a href="https://evil.invalid">Evil</a>',
        tags=["gauge", "<img src=x onerror=alert(1)>"],
        license="MIT",
    )
    malicious["provenance"]["first_party"] = True
    _submit_public(service, malicious)

    private = _manifest(
        pack_id="org.ozma.widgets.private-gallery",
        name="Private Secret Gauge",
    )
    service.submit(private)

    with _client(service) as client:
        response = client.get("/assets/gallery")

    assert response.status_code == 200
    assert response.headers["content-type"].startswith("text/html")
    assert response.headers["content-security-policy"] == CSP
    assert (
        f'<meta http-equiv="Content-Security-Policy" content="{CSP}">'
        in response.text
    )
    assert "default-src 'self'; script-src 'self'" in response.text
    assert "<script>" not in response.text
    assert "&lt;script&gt;" in response.text
    assert 'href="https://evil.invalid"' not in response.text
    assert '<a href="https://' not in response.text
    assert "<img" not in response.text
    assert "&lt;img" in response.text
    assert "Private Secret Gauge" not in response.text
    assert "Pending Gauge" not in response.text
    assert "Review Gauge" not in response.text  # pending pack
    assert f'<a href="/assets/entry/{malicious["id"]}">' in response.text
    assert "Version 1.0.0" in response.text
    assert "MIT" in response.text

    # Audit rows prove the pending pack was never approved despite appearing
    # in the same database.
    statuses = {
        row.pack_id: row.status for row in _review_rows(engine)
    }
    assert statuses[pending["id"]] == "pending"
    assert statuses[malicious["id"]] == "approved"


def test_gallery_filters(engine) -> None:
    service = _service(engine)
    widget = _manifest(
        pack_id="org.ozma.widgets.gallery-widget",
        name="Retro Gauge",
        tags=["gauge", "audio"],
    )
    theme = _manifest(
        pack_id="org.ozma.themes.gallery-theme",
        type="theme_pack",
        name="Amber Theme",
        tags=["theme", "gauge"],
    )
    _submit_public(service, widget)
    _submit_public(service, theme)

    with _client(service) as client:
        by_query = client.get("/assets/gallery", params={"q": "retro"})
        by_type = client.get("/assets/gallery", params={"type": "theme_pack"})
        by_tag = client.get("/assets/gallery", params={"tag": "audio"})
        invalid_type = client.get(
            "/assets/gallery",
            params={"type": "not_a_pack_type"},
        )
        escaped_query = client.get(
            "/assets/gallery",
            params={"q": "<script>alert(1)</script>"},
        )

    assert "Retro Gauge" in by_query.text
    assert "Amber Theme" not in by_query.text

    assert "Amber Theme" in by_type.text
    assert "Retro Gauge" not in by_type.text

    assert "Retro Gauge" in by_tag.text
    assert "Amber Theme" not in by_tag.text

    assert invalid_type.status_code == 400
    assert invalid_type.json()["detail"]["code"] == "invalid_asset"

    assert "<script>alert(1)</script>" not in escaped_query.text
    assert "&lt;script&gt;alert(1)&lt;/script&gt;" in escaped_query.text


# ── Bootstrap wiring ────────────────────────────────────────────────────────


def test_bootstrap_wires_review_configuration(tmp_path) -> None:
    app = FastAPI()
    env = {
        "OZMA_CONNECT_ASSET_DB": f"sqlite:///{tmp_path / 'review.db'}",
        "OZMA_CONNECT_ASSET_STORAGE": str(tmp_path / "objects"),
        "OZMA_CONNECT_REVIEW_REQUIRED": "1",
        "OZMA_CONNECT_REVIEW_TOKEN": "bootstrap-secret",
    }
    service = configure_assets(app, env)
    assert service is not None
    assert service.review_required is True
    assert service.review_token == "bootstrap-secret"

    app.include_router(build_router())
    manifest = _manifest(pack_id="org.ozma.widgets.bootstrap-review")
    with TestClient(app) as client:
        submitted = client.post(
            "/assets/submit",
            json={
                "manifest": manifest,
                "attestation": {"redistributable": True},
                "private": False,
            },
        )
        assert submitted.status_code == 200
        assert submitted.json()["review_status"] == "pending"
        assert client.get("/assets/search").json()["total"] == 0

        closed = client.get("/assets/review")
        allowed = client.get(
            "/assets/review",
            headers={"X-Ozma-Review-Token": "bootstrap-secret"},
        )
    assert closed.status_code == 401
    assert allowed.status_code == 200
    assert allowed.json()["total"] == 1


def test_bootstrap_review_is_off_by_default(tmp_path) -> None:
    app = FastAPI()
    env = {
        "OZMA_CONNECT_ASSET_DB": f"sqlite:///{tmp_path / 'default.db'}",
        "OZMA_CONNECT_ASSET_STORAGE": str(tmp_path / "objects"),
    }
    service = configure_assets(app, env)
    assert service is not None
    assert service.review_required is False
    assert service.review_token is None

    manifest = _manifest(pack_id="org.ozma.widgets.default-review")
    assert _submit_public(service, manifest)["review_status"] == "approved"
    assert service.search("gauge")["total"] == 1


# Do not leave the repository ``connect`` namespace cached for later controller
# tests, which expect controller/connect.py (OzmaConnect).
for _name in list(sys.modules):
    if _name == "connect" or _name.startswith("connect."):
        sys.modules.pop(_name, None)
