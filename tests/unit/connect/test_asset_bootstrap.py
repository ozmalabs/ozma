"""Tests for deploying the Connect shared asset database from the environment."""

from __future__ import annotations

import base64
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

from connect.assets.bootstrap import (  # noqa: E402
    BootstrapError,
    configure_assets,
)
from connect.assets.index import (  # noqa: E402
    HAVE_ED25519,
    Ed25519Verifier,
    verify_index,
)
from connect.assets.models import ASSET_SCHEMA  # noqa: E402
from connect.assets.router import build_router  # noqa: E402
from connect.assets.storage import FileStore  # noqa: E402

# Do not leak the import workaround into tests collected later in the session.
sys.path[:] = _ORIGINAL_SYS_PATH

pytestmark = pytest.mark.unit

_WARNING = "shared asset database is not configured"


def _manifest(
    pack_id: str = "org.ozma.widgets.bootstrap",
    version: str = "1.0.0",
) -> dict:
    return {
        "id": pack_id,
        "type": "widget_pack",
        "name": "Bootstrap Gauge",
        "version": version,
        "schema": ASSET_SCHEMA,
        "author": "Ozma",
        "description": "Bootstrap fixture",
        "license": "MIT",
        "source_url": "https://example.invalid/bootstrap",
        "tags": ["gauge", "bootstrap"],
        "assets": [
            {
                "path": "assets/gauge.svg",
                "sha256": "c" * 64,
                "size": 256,
                "mime": "image/svg+xml",
                "executable": False,
            }
        ],
        "provenance": {
            "type": "original",
            "author": "Ozma",
            "source": "fixture",
        },
    }


def _environment(tmp_path: Path, **overrides: str) -> dict[str, str]:
    env = {
        "OZMA_CONNECT_ASSET_DB": f"sqlite:///{tmp_path / 'assets.db'}",
        "OZMA_CONNECT_ASSET_STORAGE": str(tmp_path / "objects"),
    }
    env.update(overrides)
    return env


def test_environment_configures_sqlite_and_file_store_router(tmp_path) -> None:
    app = FastAPI()
    service = configure_assets(app, _environment(tmp_path))

    assert service is not None
    assert app.state.asset_service is service
    assert app.state.asset_index_channel == "stable"
    assert isinstance(service.store, FileStore)
    assert service.store.root == (tmp_path / "objects").resolve()
    assert (tmp_path / "objects").is_dir()
    assert (tmp_path / "objects").stat().st_mode & 0o777 == 0o700

    app.include_router(build_router())
    manifest = _manifest()
    submission = {
        "manifest": manifest,
        "attestation": {"redistributable": True},
        "private": False,
    }
    with TestClient(app) as client:
        submitted = client.post("/assets/submit", json=submission)
        assert submitted.status_code == 200
        assert submitted.json()["private"] is False

        search = client.get("/assets/search", params={"q": "gauge"})
        assert search.status_code == 200
        assert search.json()["total"] == 1

        entry = client.get(f"/assets/entry/{manifest['id']}")
        assert entry.status_code == 200
        assert entry.json()["id"] == manifest["id"]

        index = client.get("/assets/index/stable")
        assert index.status_code == 200
        assert index.json()["entries"][0]["id"] == manifest["id"]
        assert index.headers["x-content-sha256"]


def test_legacy_database_url_is_accepted(tmp_path) -> None:
    app = FastAPI()
    env = {
        "OZMA_CONNECT_DATABASE_URL": f"sqlite:///{tmp_path / 'legacy.db'}",
        "OZMA_CONNECT_ASSET_STORAGE": str(tmp_path / "legacy-objects"),
    }

    service = configure_assets(app, env)

    assert service is not None
    assert service.engine.url.database == str(tmp_path / "legacy.db")


def test_unset_database_url_returns_none_and_router_stays_503() -> None:
    app = FastAPI()
    app.include_router(build_router())

    env = {
        "OZMA_CONNECT_ASSET_STORAGE": "./unused",
        "OZMA_CONNECT_ASSET_SIGNING_KEY": "a" * 64,
    }
    assert configure_assets(app, env) is None
    assert getattr(app.state, "asset_service", None) is None

    with TestClient(app) as client:
        response = client.get("/assets/search")

    assert response.status_code == 503
    assert response.json()["detail"]["code"] == "asset_service_unavailable"


def test_invalid_database_url_raises_typed_sanitized_error() -> None:
    bad_url = "not-a-url"
    with pytest.raises(BootstrapError) as caught:
        configure_assets(
            FastAPI(),
            {"OZMA_CONNECT_ASSET_DB": bad_url},
        )

    assert caught.value.code == "invalid_database_url"
    assert bad_url not in str(caught.value)


def test_invalid_signing_key_raises_without_echoing_the_secret(tmp_path) -> None:
    secret = "zzzz-not-a-private-key"
    env = _environment(tmp_path, OZMA_CONNECT_ASSET_SIGNING_KEY=secret)

    with pytest.raises(BootstrapError) as caught:
        configure_assets(FastAPI(), env)

    assert caught.value.code == "invalid_signing_key"
    assert secret not in str(caught.value)


def test_repeated_configuration_reuses_the_service(tmp_path) -> None:
    app = FastAPI()
    env = _environment(tmp_path)

    first = configure_assets(app, env)
    # Even a later broken environment must not replace a working service.
    second = configure_assets(
        app,
        {"OZMA_CONNECT_ASSET_DB": "not-a-url"},
    )

    assert first is not None
    assert second is first
    assert app.state.asset_service is first


@pytest.mark.skipif(not HAVE_ED25519, reason="cryptography is not installed")
@pytest.mark.parametrize("encoding", ["base64", "hex"])
def test_ed25519_signing_key_produces_a_verifiable_index(
    tmp_path,
    encoding: str,
) -> None:
    from cryptography.hazmat.primitives import serialization
    from cryptography.hazmat.primitives.asymmetric.ed25519 import (
        Ed25519PrivateKey,
    )

    private_key = Ed25519PrivateKey.generate()
    raw = private_key.private_bytes(
        encoding=serialization.Encoding.Raw,
        format=serialization.PrivateFormat.Raw,
        encryption_algorithm=serialization.NoEncryption(),
    )
    encoded = (
        raw.hex() if encoding == "hex" else base64.b64encode(raw).decode("ascii")
    )
    env = _environment(
        tmp_path,
        OZMA_CONNECT_ASSET_SIGNING_KEY=encoded,
        OZMA_CONNECT_ASSET_SIGNING_KEY_ID="bootstrap-test",
    )
    app = FastAPI()
    service = configure_assets(app, env)
    assert service is not None

    service.submit(
        _manifest(),
        attestation={"redistributable": True},
        private=False,
    )
    snapshot = service.snapshot("stable")
    assert snapshot.index is not None
    signature = snapshot.index["signature"]
    assert signature["algorithm"] == "ed25519"
    assert signature["key_id"] == "bootstrap-test"
    assert verify_index(
        snapshot.index,
        Ed25519Verifier(private_key.public_key()),
    )


def test_bootstrap_failure_does_not_break_unrelated_routes(tmp_path) -> None:
    app = FastAPI()

    @app.get("/health")
    def health() -> dict[str, bool]:
        return {"ok": True}

    try:
        configure_assets(app, {"OZMA_CONNECT_ASSET_DB": "not-a-url"})
    except BootstrapError as exc:
        app.state.asset_bootstrap_warning = _WARNING
        app.state.asset_bootstrap_error = exc.code

    app.include_router(build_router())
    with TestClient(app) as client:
        assert client.get("/health").status_code == 200
        assert client.get("/assets/search").status_code == 503

    assert app.state.asset_bootstrap_error == "invalid_database_url"
    assert app.state.asset_bootstrap_warning == _WARNING

# Do not leave the repository ``connect`` namespace cached for later controller
# tests, which expect controller/connect.py (OzmaConnect).
for _name in list(sys.modules):
    if _name == "connect" or _name.startswith("connect."):
        sys.modules.pop(_name, None)
