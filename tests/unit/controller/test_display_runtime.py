# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for display-platform runtime assembly and feature gating."""

from __future__ import annotations

import base64
import json
from pathlib import Path

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

from asset_repository import AssetRepository, build_router as build_asset_router
from display_config import DisplayConfig, DisplayConfigError
from display_import_api import build_router as build_import_router
from display_import_store import ImportStore
from display_runtime import (
    DisplayDisabledError,
    build_config_router,
    build_runtime,
)

pytestmark = pytest.mark.unit

FIXTURES = Path(__file__).parents[2] / "fixtures" / "display_imports"
HA_SOURCE = FIXTURES / "ha_dashboard.yaml"

_DISPLAY_ENV_KEYS = (
    "OZMA_DISPLAY_IR_ENABLED",
    "OZMA_DISPLAY_ASSET_REPOSITORY_ENABLED",
    "OZMA_DISPLAY_SYNC_MODE",
    "OZMA_DISPLAY_FONTS_TRANSLATION_ENABLED",
    "OZMA_DISPLAY_IMPORTERS_ENABLED",
    "OZMA_DISPLAY_ASSET_INDEX_URL",
    "OZMA_DISPLAY_CACHE_DIR",
    "OZMA_DISPLAY_ALLOW_UNSIGNED_PACKS",
    "OZMA_DISPLAY_SIGNATURE_REQUIRED",
    "OZMA_DISPLAY_TRUSTED_KEYS",
    "OZMA_DISPLAY_MAX_PACK_BYTES",
    "OZMA_DISPLAY_MAX_IMPORT_BYTES",
)


@pytest.fixture(autouse=True)
def _clean_display_env(monkeypatch: pytest.MonkeyPatch) -> None:
    for key in _DISPLAY_ENV_KEYS:
        monkeypatch.delenv(key, raising=False)


@pytest.fixture(autouse=True)
def _isolate_mesh_config(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    """Keep build_app from writing controller/mesh_network.json in the repo."""
    import mesh_network

    monkeypatch.setattr(mesh_network, "CONFIG_PATH", tmp_path / "mesh_network.json")


def _no_network(*args, **kwargs):  # pragma: no cover - only fails when called
    raise AssertionError("runtime construction must not touch the network")


def _client(app: FastAPI) -> TestClient:
    return TestClient(app)


def _assert_disabled(response) -> None:
    assert response.status_code == 503, response.text
    assert response.json()["detail"]["code"] == "display_disabled"


def _build_app(tmp_path: Path, *, display_config=None):
    from api import build_app
    from auth import AuthConfig
    from scenarios import ScenarioManager
    from state import AppState
    from stream import StreamManager

    state = AppState()
    streams = StreamManager(state, static_dir=tmp_path / "streams")
    scenarios = ScenarioManager(tmp_path / "scenarios.json", state)
    return build_app(
        state,
        scenarios,
        streams=streams,
        auth_config=AuthConfig(enabled=False),
        display_config=display_config,
    )


# ── Runtime assembly ────────────────────────────────────────────────────────


def test_runtime_defaults_from_env_are_offline_and_conservative(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    monkeypatch.setattr("asset_repository.urlopen", _no_network)
    config = DisplayConfig.from_env()
    runtime = build_runtime(config, cache_dir=tmp_path / "cache")

    assert runtime.config == config
    assert runtime.enabled("ir") is False
    assert runtime.enabled("asset_repository") is False
    assert runtime.enabled("fonts_translation") is False
    assert runtime.enabled("importers") is False
    assert runtime.config.sync_mode == "offline"
    assert runtime.asset_repository.policy.mode == "offline"
    assert runtime.asset_repository.cache.root == tmp_path / "cache"
    assert runtime.import_store.root == tmp_path / "cache" / "imports"
    assert (
        runtime.asset_repository.limits.max_total_bytes
        == config.max_pack_bytes
    )
    assert runtime.import_store.max_import_bytes == config.max_import_bytes


def test_runtime_env_overrides_flow_into_components(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    cache = tmp_path / "env-cache"
    monkeypatch.setenv("OZMA_DISPLAY_ASSET_REPOSITORY_ENABLED", "on")
    monkeypatch.setenv("OZMA_DISPLAY_IMPORTERS_ENABLED", "1")
    monkeypatch.setenv("OZMA_DISPLAY_SYNC_MODE", "installed")
    monkeypatch.setenv("OZMA_DISPLAY_CACHE_DIR", str(cache))
    monkeypatch.setenv("OZMA_DISPLAY_MAX_PACK_BYTES", "4096")
    monkeypatch.setenv("OZMA_DISPLAY_MAX_IMPORT_BYTES", "2048")
    monkeypatch.setenv("OZMA_DISPLAY_SIGNATURE_REQUIRED", "yes")
    trust_json = json.dumps(
        {
            "default_key_id": "key-1",
            "keys": [
                {
                    "key_id": "key-1",
                    "algorithm": "hmac-sha256",
                    "public_key": "hex:" + "11" * 32,
                }
            ],
        }
    )
    monkeypatch.setenv("OZMA_DISPLAY_TRUSTED_KEYS", trust_json)
    monkeypatch.setattr("asset_repository.urlopen", _no_network)

    runtime = build_runtime(DisplayConfig.from_env())

    assert runtime.enabled("asset_repository") is True
    assert runtime.enabled("importers") is True
    assert runtime.config.sync_mode == "installed"
    assert runtime.asset_repository.policy.mode == "installed"
    assert runtime.asset_repository.cache.root == cache
    assert runtime.asset_repository.limits.max_total_bytes == 4096
    assert runtime.asset_repository.require_signature is True
    assert runtime.asset_repository.signature_verifier is not None
    assert runtime.import_store.max_import_bytes == 2048
    assert runtime.config.trusted_keys == trust_json


def test_runtime_mapping_overrides_apply_to_repository_and_store(
    tmp_path: Path,
) -> None:
    packs = tmp_path / "packs"
    cache = tmp_path / "cache"
    runtime = build_runtime(
        {
            "OZMA_DISPLAY_ASSET_REPOSITORY_ENABLED": "yes",
            "OZMA_DISPLAY_IMPORTERS_ENABLED": "on",
            "OZMA_DISPLAY_SYNC_MODE": "manual",
            "OZMA_DISPLAY_ALLOW_UNSIGNED_PACKS": "false",
            "OZMA_DISPLAY_SIGNATURE_REQUIRED": "true",
            "OZMA_DISPLAY_ASSET_INDEX_URL": "https://assets.example.test/index.json",
            "OZMA_DISPLAY_MAX_PACK_BYTES": "1024",
            "OZMA_DISPLAY_MAX_IMPORT_BYTES": "512",
        },
        packs_dir=packs,
        cache_dir=cache,
    )

    assert runtime.enabled("asset_repository") is True
    assert runtime.enabled("importers") is True
    assert runtime.asset_repository.packs_dir == packs
    assert runtime.asset_repository.cache.root == cache
    assert runtime.asset_repository.policy.mode == "manual"
    assert runtime.asset_repository.allow_unsigned is False
    assert runtime.asset_repository.require_signature is True
    assert runtime.asset_repository.index_url == (
        "https://assets.example.test/index.json"
    )
    assert runtime.asset_repository.limits.max_total_bytes == 1024
    assert runtime.asset_repository.limits.max_file_bytes == 1024
    assert runtime.import_store.max_import_bytes == 512


def test_status_is_sanitized_and_reports_counts(tmp_path: Path) -> None:
    secret = "22" * 32
    runtime = build_runtime(
        DisplayConfig(
            asset_repository_enabled=True,
            importers_enabled=True,
            asset_index_url=f"https://assets.example.test/index.json?token={secret}",
            cache_dir=str(tmp_path / "cache"),
            trusted_keys=json.dumps(
                {
                    "default_key_id": "status-key",
                    "keys": [
                        {
                            "key_id": "status-key",
                            "algorithm": "hmac-sha256",
                            "public_key": secret,
                        }
                    ],
                }
            ),
            max_pack_bytes=2048,
            max_import_bytes=4096,
        )
    )
    runtime.import_store.save("dashboard.yaml", HA_SOURCE.read_bytes())

    status = runtime.status()
    rendered = json.dumps(status)

    assert status["features"] == {
        "ir": False,
        "asset_repository": True,
        "fonts_translation": False,
        "importers": True,
    }
    assert status["feature_flags"]["display.asset_sync.mode"] == "offline"
    assert status["sync_mode"] == "offline"
    assert status["asset_index_url_configured"] is True
    assert status["cache_dir_configured"] is True
    assert status["trusted_keys_configured"] is True
    assert status["limits"] == {
        "max_pack_bytes": 2048,
        "max_import_bytes": 4096,
    }
    assert status["counts"]["imports"] == 1
    assert status["counts"]["asset_index_packs"] == 0
    assert secret not in rendered
    assert "assets.example.test" not in rendered
    assert str(tmp_path) not in rendered


def test_require_raises_typed_disabled_error(tmp_path: Path) -> None:
    runtime = build_runtime(DisplayConfig(cache_dir=str(tmp_path / "cache")))

    assert runtime.enabled("asset_repository_enabled") is False
    with pytest.raises(DisplayDisabledError) as excinfo:
        runtime.require("asset_repository")

    error = excinfo.value
    assert error.feature == "asset_repository"
    assert error.status_code == 503
    assert error.code == "display_disabled"
    assert error.to_detail()["code"] == "display_disabled"

    with pytest.raises(ValueError):
        runtime.enabled("not-a-feature")


def test_zero_import_budget_is_a_typed_configuration_error() -> None:
    with pytest.raises(DisplayConfigError) as excinfo:
        build_runtime(DisplayConfig(max_import_bytes=0))

    assert excinfo.value.variable == "OZMA_DISPLAY_MAX_IMPORT_BYTES"


def test_invalid_env_is_a_typed_configuration_error(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    monkeypatch.setenv("OZMA_DISPLAY_MAX_IMPORT_BYTES", "64MB")

    with pytest.raises(DisplayConfigError) as excinfo:
        DisplayConfig.from_env()
    assert excinfo.value.variable == "OZMA_DISPLAY_MAX_IMPORT_BYTES"

    # build_app surfaces the same typed error rather than a bare ValueError.
    with pytest.raises(DisplayConfigError):
        _build_app(tmp_path)


# ── Routers and feature gates ───────────────────────────────────────────────


def test_config_router_is_available_regardless_of_flags(tmp_path: Path) -> None:
    runtime = build_runtime(
        DisplayConfig(cache_dir=str(tmp_path / "cache"))
    )
    app = FastAPI()
    app.include_router(build_config_router(runtime))

    response = _client(app).get("/api/v1/display/config")

    assert response.status_code == 200, response.text
    payload = response.json()
    assert payload["features"]["asset_repository"] is False
    assert payload["features"]["importers"] is False
    assert payload["limits"]["max_pack_bytes"] > 0


def test_disabled_asset_repository_returns_typed_503(tmp_path: Path) -> None:
    repository = AssetRepository(
        packs_dir=tmp_path / "packs",
        cache_dir=tmp_path / "cache",
    )
    app = FastAPI()
    app.include_router(build_asset_router(repository, enabled=False))

    client = _client(app)
    _assert_disabled(client.get("/api/v1/asset-db/search"))
    _assert_disabled(client.get("/api/v1/asset-db/stats"))
    _assert_disabled(client.post("/api/v1/asset-db/sync"))


def test_enabled_asset_repository_still_serves_with_temp_dirs(
    tmp_path: Path,
) -> None:
    repository = AssetRepository(
        packs_dir=tmp_path / "packs",
        cache_dir=tmp_path / "cache",
    )
    app = FastAPI()
    app.include_router(build_asset_router(repository, enabled=True))

    response = _client(app).get("/api/v1/asset-db/search", params={"q": "demo"})

    assert response.status_code == 200, response.text
    assert response.json()["entries"] == []


def test_disabled_importers_block_posts_but_keep_reads(tmp_path: Path) -> None:
    store = ImportStore(tmp_path / "imports")
    manifest = store.save("dashboard.yaml", HA_SOURCE.read_bytes())

    app = FastAPI()
    app.include_router(build_import_router(store, enabled=False))
    client = _client(app)

    payload = {
        "name": "dashboard.yaml",
        "format": "home_assistant",
        "data_base64": base64.b64encode(HA_SOURCE.read_bytes()).decode("ascii"),
    }
    _assert_disabled(client.post("/api/v1/display/import", json=payload))

    listed = client.get("/api/v1/display/imports")
    assert listed.status_code == 200
    assert listed.json()["count"] == 1
    assert client.get(f"/api/v1/display/imports/{manifest['id']}").status_code == 200

    importers = client.get("/api/v1/display/importers")
    assert importers.status_code == 200
    assert importers.json() == {"enabled": False, "importers": []}


def test_enabled_importers_accept_a_fixture(tmp_path: Path) -> None:
    store = ImportStore(tmp_path / "imports")
    app = FastAPI()
    app.include_router(build_import_router(store, enabled=True))
    client = _client(app)

    importers = client.get("/api/v1/display/importers")
    assert importers.status_code == 200
    assert importers.json()["enabled"] is True
    assert importers.json()["importers"]

    response = client.post(
        "/api/v1/display/import",
        json={
            "name": "dashboard.yaml",
            "format": "home_assistant",
            "data_base64": base64.b64encode(HA_SOURCE.read_bytes()).decode("ascii"),
        },
    )
    assert response.status_code == 201, response.text
    assert response.json()["import"]["private"] is True

    listed = client.get("/api/v1/display/imports")
    assert listed.status_code == 200
    assert listed.json()["count"] == 1


# ── build_app smoke and wiring ──────────────────────────────────────────────


def test_build_app_default_env_starts_with_flags_off(tmp_path: Path) -> None:
    client = _client(_build_app(tmp_path))

    config = client.get("/api/v1/display/config")
    assert config.status_code == 200, config.text
    assert config.json()["features"]["asset_repository"] is False
    assert config.json()["features"]["importers"] is False

    _assert_disabled(client.get("/api/v1/asset-db/search"))
    _assert_disabled(
        client.post(
            "/api/v1/display/import",
            json={"name": "x.yaml", "format": "home_assistant", "text": "cards: []"},
        )
    )


def test_build_app_enabled_flags_reach_runtime_routers(tmp_path: Path) -> None:
    config = DisplayConfig(
        asset_repository_enabled=True,
        importers_enabled=True,
        cache_dir=str(tmp_path / "cache"),
    )
    client = _client(_build_app(tmp_path, display_config=config))

    assert client.get("/api/v1/asset-db/search").status_code == 200
    assert client.get("/api/v1/display/importers").json()["enabled"] is True

    response = client.post(
        "/api/v1/display/import",
        json={
            "name": "dashboard.yaml",
            "format": "home_assistant",
            "data_base64": base64.b64encode(HA_SOURCE.read_bytes()).decode("ascii"),
        },
    )
    assert response.status_code == 201, response.text
