# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Tests for private display import storage and API."""

from __future__ import annotations

import base64
import hashlib
from pathlib import Path

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

from display_import_api import build_router
from display_import_store import (
    ImportStore,
    ImportStoreError,
    ImportTooLargeError,
)
from display_importers.base import ImportError as SourceImportError

FIXTURES = Path(__file__).parents[2] / "fixtures" / "display_imports"
HA_SOURCE = FIXTURES / "ha_dashboard.yaml"
RAINMETER_SOURCE = FIXTURES / "rainmeter_skin.ini"


def _sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _client(store: ImportStore) -> TestClient:
    app = FastAPI()
    app.include_router(build_router(store))
    return TestClient(app)


def test_store_saves_private_lossless_pack(tmp_path: Path) -> None:
    data = HA_SOURCE.read_bytes()
    store = ImportStore(tmp_path / "imports")
    manifest = store.save("dashboard.yaml", data)

    assert manifest["private"] is True
    assert manifest["published"] is False
    assert manifest["format"].startswith("home_assistant")
    assert manifest["raw"]["sha256"] == _sha256(data)
    assert manifest["raw"]["size"] == len(data)
    assert manifest["source_capture"]["lossless"] is True
    assert manifest["source"]["source_hash"] == _sha256(data)
    assert manifest["widgets"]
    assert store.load_raw(manifest["id"]) == data

    # Every declared asset is retrievable and hash-checked.
    for asset in manifest["assets"]:
        assert store.load_asset(manifest["id"], asset["sha256"])

    assert len(store.list_imports()) == 1
    assert store.get(manifest["id"])["id"] == manifest["id"]


def test_store_is_idempotent_and_can_delete(tmp_path: Path) -> None:
    data = RAINMETER_SOURCE.read_bytes()
    store = ImportStore(tmp_path / "imports")
    first = store.save("skin.ini", data, format="rainmeter")
    second = store.save("skin.ini", data, format="rainmeter")
    assert first["id"] == second["id"]
    assert first["created_at"] == second["created_at"]
    assert store.delete(first["id"]) is True
    assert store.list_imports() == []
    with pytest.raises(ImportStoreError):
        store.get(first["id"])


def test_store_rejects_oversize_and_bad_ids(tmp_path: Path) -> None:
    store = ImportStore(tmp_path / "imports", max_import_bytes=10)
    with pytest.raises(ImportTooLargeError):
        store.save("big.yaml", b"x" * 64)
    with pytest.raises(ImportStoreError):
        store.get("../../etc/passwd")
    with pytest.raises(ImportStoreError):
        store.load_raw("imp-000000000000000000000000")


def test_store_preserves_raw_and_unknown_content(tmp_path: Path) -> None:
    data = b"checkstyle:\n  custom_extension: keep-me\ncards: []\n"
    store = ImportStore(tmp_path / "imports")
    manifest = store.save("unknown.yaml", data, format="home_assistant")
    assert store.load_raw(manifest["id"]) == data
    assert manifest["source"]["unknown"] is not None
    assert manifest["source_capture"]["lossless"] is True


def test_api_import_list_get_raw_and_delete(tmp_path: Path) -> None:
    store = ImportStore(tmp_path / "imports")
    client = _client(store)
    data = HA_SOURCE.read_bytes()

    response = client.post(
        "/api/v1/display/import",
        json={
            "name": "dashboard.yaml",
            "format": "home_assistant",
            "data_base64": base64.b64encode(data).decode("ascii"),
        },
    )
    assert response.status_code == 201, response.text
    manifest = response.json()["import"]
    assert manifest["private"] is True
    assert manifest["published"] is False

    listed = client.get("/api/v1/display/imports")
    assert listed.status_code == 200
    assert listed.json()["count"] == 1

    fetched = client.get(f"/api/v1/display/imports/{manifest['id']}")
    assert fetched.status_code == 200
    assert fetched.json()["import"]["id"] == manifest["id"]

    raw = client.get(f"/api/v1/display/imports/{manifest['id']}/raw")
    assert raw.status_code == 200
    assert raw.content == data

    asset = manifest["assets"][0]
    stored = client.get(f"/api/v1/display/imports/{manifest['id']}/assets/{asset['sha256']}")
    assert stored.status_code == 200
    assert hashlib.sha256(stored.content).hexdigest() == asset["sha256"]

    deleted = client.delete(f"/api/v1/display/imports/{manifest['id']}")
    assert deleted.status_code == 200
    assert client.get(f"/api/v1/display/imports/{manifest['id']}").status_code == 404


def test_api_accepts_text_and_rejects_public_or_unknown(tmp_path: Path) -> None:
    store = ImportStore(tmp_path / "imports")
    client = _client(store)

    response = client.post(
        "/api/v1/display/import",
        json={"name": "skin.ini", "format": "rainmeter", "text": RAINMETER_SOURCE.read_text()},
    )
    assert response.status_code == 201, response.text

    public = client.post(
        "/api/v1/display/import",
        json={"name": "x.yaml", "format": "home_assistant", "text": "cards: []", "private": False},
    )
    assert public.status_code == 400
    assert public.json()["detail"]["code"] == "invalid_import"

    unknown = client.post(
        "/api/v1/display/import",
        json={"name": "x.unknown", "format": "not-a-format", "text": "x"},
    )
    assert unknown.status_code == 400
    assert unknown.json()["detail"]["code"] == "import_error" or unknown.json()["detail"]["code"] == "invalid_import"


def test_api_rejects_oversize_and_bad_base64(tmp_path: Path) -> None:
    store = ImportStore(tmp_path / "imports", max_import_bytes=8)
    client = _client(store)

    oversize = client.post(
        "/api/v1/display/import",
        json={"name": "big.yaml", "data_base64": base64.b64encode(b"x" * 64).decode("ascii")},
    )
    assert oversize.status_code == 413

    invalid = client.post(
        "/api/v1/display/import",
        json={"name": "bad.yaml", "data_base64": "not base64!"},
    )
    assert invalid.status_code == 400


def test_importers_endpoint_lists_formats(tmp_path: Path) -> None:
    client = _client(ImportStore(tmp_path / "imports"))
    response = client.get("/api/v1/display/importers")
    assert response.status_code == 200
    formats = {item["format"] for item in response.json()["importers"]}
    assert any(item.startswith("home_assistant") for item in formats)
    assert any(item.startswith("rainmeter") for item in formats)


def test_registry_rejects_a_format_with_no_module(tmp_path: Path) -> None:
    store = ImportStore(tmp_path / "imports")
    with pytest.raises(SourceImportError):
        store.save("x.unknown", b"not a source", format="unknown-format")
