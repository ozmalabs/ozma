"""Marketplace shell, static asset, and review-panel tests."""

from __future__ import annotations

import sys
from pathlib import Path

import pytest

# The unit-test collector may put ``tests/unit/connect`` on sys.path, shadowing
# the repository's namespace package.  Prefer the repo root.
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
from sqlalchemy import event  # noqa: E402

from connect.assets.gallery import (  # noqa: E402
    CSP,
    STATIC_CACHE_CONTROL,
    STATIC_DIR,
)
from connect.assets.index import AssetService  # noqa: E402
from connect.assets.models import (  # noqa: E402
    ASSET_SCHEMA,
    create_schema,
    create_sqlite_engine,
)
from connect.assets.router import build_router  # noqa: E402
from connect.assets.storage import MemoryStore  # noqa: E402

sys.path[:] = _ORIGINAL_SYS_PATH

pytestmark = pytest.mark.unit


@pytest.fixture
def engine(tmp_path):
    database = tmp_path / "gallery.db"
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
    pack_id: str = "org.ozma.widgets.market",
    version: str = "1.0.0",
    **overrides,
) -> dict:
    manifest = {
        "id": pack_id,
        "type": "widget_pack",
        "name": "Market Gauge",
        "version": version,
        "schema": ASSET_SCHEMA,
        "author": "Ozma",
        "description": "A marketplace fixture",
        "license": "MIT",
        "source_url": "https://example.invalid/source",
        "tags": ["gauge", "audio"],
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
        "vibe": {"tone": "retro", "accent": "#336699"},
        "shape": {"layout": "radial"},
        "coherence": "exact",
        "published_at": "2026-01-01T00:00:00+00:00",
        "yanked": False,
    }
    manifest.update(overrides)
    return manifest


def _service(engine, *, review_required: bool = False, token=None):
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


def _approved_manifest(**overrides) -> dict:
    manifest = _manifest(**overrides)
    manifest["provenance"]["first_party"] = True
    return manifest


def test_marketplace_shell_is_escaped_and_sets_strict_csp(engine) -> None:
    service = _service(engine, review_required=True, token="gallery-secret")

    pending = _manifest(
        pack_id="org.ozma.widgets.pending-market",
        name="Pending Gauge",
    )
    _submit_public(service, pending)

    private = _manifest(
        pack_id="org.ozma.widgets.private-market",
        name="Private Secret Gauge",
    )
    service.submit(private)

    approved = _approved_manifest(
        pack_id="org.ozma.widgets.approved-market",
        name='<script>alert("x")</script> Gauge',
        author='<a href="https://evil.invalid">Evil</a>',
        tags=["gauge", "<img src=x onerror=alert(1)>"],
        preview="javascript:alert(1)",
    )
    _submit_public(service, approved)

    with _client(service) as client:
        response = client.get("/assets/marketplace")

    assert response.status_code == 200
    assert response.headers["content-type"].startswith("text/html")
    assert response.headers["content-security-policy"] == CSP
    assert (
        f'<meta http-equiv="Content-Security-Policy" content="{CSP}">'
        in response.text
    )
    assert "script-src 'self'" in response.text
    assert '<link rel="stylesheet" href="/assets/static/marketplace.css">' in response.text
    assert '<script src="/assets/static/marketplace.js" defer></script>' in response.text
    # Only the external script element, never inline executable source.
    assert "<script>" not in response.text
    assert "&lt;script&gt;" in response.text
    assert 'href="https://evil.invalid"' not in response.text
    assert "<img" not in response.text
    assert "&lt;img" in response.text
    assert "Private Secret Gauge" not in response.text
    assert "Pending Gauge" not in response.text
    assert f'/assets/entry/{approved["id"]}' in response.text
    assert "Version 1.0.0" in response.text
    assert "gallery-secret" not in response.text


def test_marketplace_query_is_escaped_and_bounded(engine) -> None:
    service = _service(engine)
    _submit_public(service, _manifest(name="Retro Gauge"))

    with _client(service) as client:
        escaped = client.get(
            "/assets/marketplace",
            params={"q": "<script>alert(1)</script>"},
        )
        bounded = client.get(
            "/assets/marketplace",
            params={"q": "x" * 500},
        )

    assert escaped.status_code == 200
    assert "<script>alert(1)</script>" not in escaped.text
    assert "&lt;script&gt;alert(1)&lt;/script&gt;" in escaped.text

    assert bounded.status_code == 200
    assert ("x" * 200) in bounded.text
    assert ("x" * 201) not in bounded.text


def test_gallery_remains_a_backward_compatible_alias(engine) -> None:
    service = _service(engine)
    manifest = _manifest(name="Alias Gauge")
    _submit_public(service, manifest)

    with _client(service) as client:
        gallery = client.get("/assets/gallery")
        marketplace = client.get("/assets/marketplace")

    assert gallery.status_code == 200
    assert marketplace.status_code == 200
    assert gallery.headers["content-security-policy"] == CSP
    assert marketplace.headers["content-security-policy"] == CSP
    assert gallery.text == marketplace.text
    assert manifest["id"] in gallery.text
    assert f'/assets/entry/{manifest["id"]}' in gallery.text


def test_static_assets_are_served_with_mime_and_cache_headers(engine) -> None:
    service = _service(engine)
    with _client(service) as client:
        javascript = client.get("/assets/static/marketplace.js")
        stylesheet = client.get("/assets/static/marketplace.css")

    assert javascript.status_code == 200
    assert javascript.headers["content-type"].startswith("text/javascript")
    assert javascript.headers["cache-control"] == STATIC_CACHE_CONTROL
    assert javascript.headers["x-content-type-options"] == "nosniff"
    assert b"textContent" in javascript.content

    assert stylesheet.status_code == 200
    assert stylesheet.headers["content-type"].startswith("text/css")
    assert stylesheet.headers["cache-control"] == STATIC_CACHE_CONTROL
    assert stylesheet.headers["x-content-type-options"] == "nosniff"
    assert b"prefers-reduced-motion" in stylesheet.content


@pytest.mark.parametrize(
    "path",
    [
        "/assets/static/missing.js",
        "/assets/static/../gallery.py",
        "/assets/static/%2e%2e%2fgallery.py",
        "/assets/static/..%2fgallery.py",
        "/assets/static/marketplace.js%00.txt",
    ],
)
def test_static_assets_reject_unknown_paths_and_traversal(engine, path) -> None:
    service = _service(engine)
    with _client(service) as client:
        response = client.get(path)
    assert response.status_code == 404


def test_marketplace_static_sources_follow_security_rules() -> None:
    javascript = (STATIC_DIR / "marketplace.js").read_text(encoding="utf-8")
    stylesheet = (STATIC_DIR / "marketplace.css").read_text(encoding="utf-8")

    assert "textContent" in javascript
    assert "sessionStorage" in javascript
    assert "X-Ozma-Review-Token" in javascript
    assert "localStorage" not in javascript
    assert "innerHTML" not in javascript
    assert "outerHTML" not in javascript
    assert "document.write" not in javascript
    assert "eval(" not in javascript
    assert "new Function" not in javascript
    assert "http://" not in javascript
    assert "https://" not in javascript

    assert "@import" not in stylesheet
    assert "url(" not in stylesheet
    assert "http://" not in stylesheet
    assert "https://" not in stylesheet
    assert "prefers-reduced-motion" in stylesheet


def test_review_panel_token_is_not_embedded_and_api_auth_is_enforced(
    engine,
) -> None:
    service = _service(
        engine,
        review_required=True,
        token="operator-only-secret",
    )
    manifest = _manifest(pack_id="org.ozma.widgets.review-panel")
    _submit_public(service, manifest)

    with _client(service) as client:
        page = client.get("/assets/marketplace")
        missing = client.get("/assets/review")
        invalid = client.get(
            "/assets/review",
            headers={"X-Ozma-Review-Token": "wrong"},
        )
        allowed = client.get(
            "/assets/review",
            headers={"X-Ozma-Review-Token": "operator-only-secret"},
        )
        missing_post = client.post(
            f"/assets/review/{manifest['id']}/approve",
            json={},
        )
        approved = client.post(
            f"/assets/review/{manifest['id']}/approve",
            json={"note": "checked"},
            headers={"X-Ozma-Review-Token": "operator-only-secret"},
        )

    assert page.status_code == 200
    assert "operator-only-secret" not in page.text
    assert 'value="operator-only-secret"' not in page.text
    assert 'id="review-token"' in page.text
    assert 'type="password"' in page.text
    assert "sessionStorage" not in page.text

    assert missing.status_code == 401
    assert missing.json()["detail"]["code"] == "review_token_missing"
    assert invalid.status_code == 403
    assert invalid.json()["detail"]["code"] == "review_token_invalid"
    assert allowed.status_code == 200
    assert allowed.json()["total"] == 1
    assert missing_post.status_code == 401
    assert approved.status_code == 200
    assert approved.json()["status"] == "approved"


def test_search_json_supports_sort_and_pagination(engine) -> None:
    service = _service(engine)
    for name, author in (
        ("Apple Gauge", "Zed"),
        ("Banana Gauge", "Amy"),
        ("Cherry Gauge", "Mia"),
    ):
        _submit_public(
            service,
            _manifest(
                pack_id="org.ozma.widgets." + name.split()[0].lower(),
                name=name,
                author=author,
            ),
        )

    with _client(service) as client:
        first = client.get(
            "/assets/search",
            params={"sort": "name_desc", "limit": 1, "offset": 0},
        )
        second = client.get(
            "/assets/search",
            params={"sort": "name_desc", "limit": 1, "offset": 1},
        )
        by_author = client.get(
            "/assets/search",
            params={"sort": "author", "limit": 3},
        )

    assert first.status_code == 200
    assert first.json()["total"] == 3
    assert first.json()["count"] == 1
    assert first.json()["entries"][0]["name"] == "Cherry Gauge"
    assert second.json()["entries"][0]["name"] == "Banana Gauge"
    assert [
        item["name"] for item in by_author.json()["entries"]
    ] == ["Banana Gauge", "Cherry Gauge", "Apple Gauge"]


def test_marketplace_stays_public_when_review_is_unavailable(engine) -> None:
    service = _service(engine, review_required=False, token=None)
    _submit_public(service, _manifest(name="Public Gauge"))

    with _client(service) as client:
        page = client.get("/assets/marketplace")
        review = client.get("/assets/review")

    assert page.status_code == 200
    assert "Public Gauge" in page.text
    assert review.status_code == 503
    assert review.json()["detail"]["code"] == "review_unavailable"


# Do not leave the repository ``connect`` namespace cached for later controller
# tests, which expect controller/connect.py (OzmaConnect).
for _name in list(sys.modules):
    if _name == "connect" or _name.startswith("connect."):
        sys.modules.pop(_name, None)
