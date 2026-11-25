"""Cross-service sync: a signed Connect index loads into the controller cache."""

from __future__ import annotations

import sys
from pathlib import Path
from types import SimpleNamespace

import pytest
from sqlalchemy import event

_ORIGINAL_SYS_PATH = list(sys.path)

_REPO_ROOT = Path(__file__).resolve().parents[3]
_CONTROLLER = _REPO_ROOT / "controller"
_SHADOW = Path(__file__).resolve().parents[0]
# The root conftest puts controller/ first, where controller/connect.py would
# shadow the repository's ``connect`` namespace package. Import Connect first,
# then add controller/ back for the controller-side modules.
sys.path[:] = [
    item for item in sys.path
    if not item
    or Path(item).resolve() != _SHADOW
    and not (Path(item) / "connect.py").exists()
]
sys.path.insert(0, str(_REPO_ROOT))
for name in list(sys.modules):
    if name == "connect" or name.startswith("connect."):
        sys.modules.pop(name, None)

from connect.assets.index import (  # noqa: E402
    AssetService,
    HMACSigner,
    HMACVerifier,
    build_index,
    index_bytes,
)
from connect.assets.models import (  # noqa: E402
    ASSET_SCHEMA,
    create_schema,
    create_sqlite_engine,
)
from connect.assets.storage import MemoryStore  # noqa: E402

sys.path.insert(0, str(_CONTROLLER))

from asset_repository import (  # noqa: E402
    AssetRepository,
    AssetSignatureError,
    AssetSyncPolicy,
)

pytestmark = pytest.mark.unit

_KEY = b"cross-service-signing-key"


def _manifest() -> dict:
    return {
        "id": "org.ozma.widgets.cross-service",
        "type": "widget_pack",
        "name": "Cross Service",
        "version": "2.1.0",
        "schema": ASSET_SCHEMA,
        "author": "Ozma",
        "description": "Cross-service sync fixture",
        "license": "MIT",
        "tags": ["gauge"],
        "assets": [
            {
                "path": "assets/gauge.svg",
                "sha256": "b" * 64,
                "size": 64,
                "mime": "image/svg+xml",
                "executable": False,
            }
        ],
        "provenance": {"type": "original", "author": "Ozma", "source": "fixture"},
        "vibe": {"mood": ["retro"]},
        "shape": {"family": "radial"},
        "coherence": {"score": 0.8},
        "published_at": "2026-01-01T00:00:00+00:00",
    }


@pytest.fixture
def engine(tmp_path):
    database = tmp_path / "assets.db"
    engine = create_sqlite_engine(database)

    @event.listens_for(engine, "connect")
    def _foreign_keys(connection, _record):
        cursor = connection.cursor()
        cursor.execute("PRAGMA foreign_keys=ON")
        cursor.close()

    create_schema(engine)
    yield engine
    engine.dispose()


def _signed_index(engine) -> dict:
    signer = HMACSigner(_KEY, key_id="cross")
    service = AssetService(engine, store=MemoryStore(), signer=signer)
    service.submit(
        _manifest(),
        attestation={"redistributable": True},
        private=False,
    )
    return build_index(
        engine,
        channel="stable",
        generated_at="2026-02-01T00:00:00+00:00",
        signer=signer,
    )


def _repository(tmp_path) -> AssetRepository:
    verifier = HMACVerifier(_KEY)
    return AssetRepository(
        packs_dir=tmp_path / "packs",
        cache_dir=tmp_path / "cache",
        allow_unsigned=False,
        signature_verifier=lambda payload, signature, _files: verifier.verify(payload, signature),
    )


def test_signed_connect_index_loads_in_controller(engine, tmp_path) -> None:
    index = _signed_index(engine)
    body = index_bytes(index)
    repo = _repository(tmp_path)

    def transport(url, headers=None):
        return SimpleNamespace(status_code=200, headers={"ETag": index["etag"]}, content=body)

    result = repo.sync(
        "https://connect.test/assets/index/stable",
        transport=transport,
        policy={"mode": "full", "verify_signatures": True},
    )

    assert result["synced"] is True
    assert result["updated"] is True
    assert repo.index.etag == index["etag"]
    assert repo.index.signature
    assert repo.index.signature_meta.get("key_id") == "cross"
    entry = repo.index.entry("org.ozma.widgets.cross-service")
    assert entry is not None
    assert entry.version == "2.1.0"
    assert entry.license == "MIT"
    assert entry.assets and entry.assets[0].sha256 == "b" * 64
    assert entry.assets[0].extra.get("id")


def test_tampered_signed_index_is_rejected(engine, tmp_path) -> None:
    index = _signed_index(engine)
    tampered = dict(index)
    tampered["entries"] = [dict(index["entries"][0], name="Tampered")]
    body = index_bytes(tampered)
    repo = _repository(tmp_path)

    def transport(url, headers=None):
        return SimpleNamespace(status_code=200, headers={}, content=body)

    with pytest.raises(AssetSignatureError):
        repo.sync(
            "https://connect.test/assets/index/stable",
            transport=transport,
            policy={"mode": "full", "verify_signatures": True},
        )


def test_unsigned_index_is_rejected_when_policy_requires_signatures(engine, tmp_path) -> None:
    index = _signed_index(engine)
    unsigned = {key: value for key, value in index.items() if key != "signature"}
    body = index_bytes(unsigned)
    repo = _repository(tmp_path)

    def transport(url, headers=None):
        return SimpleNamespace(status_code=200, headers={}, content=body)

    with pytest.raises(AssetSignatureError):
        repo.sync(
            "https://connect.test/assets/index/stable",
            transport=transport,
            policy=AssetSyncPolicy(mode="full", verify_signatures=True),
        )


def test_304_keeps_the_cached_index(engine, tmp_path) -> None:
    index = _signed_index(engine)
    repo = _repository(tmp_path)
    repo.index.etag = index["etag"]

    def transport(url, headers=None):
        assert headers.get("If-None-Match") == index["etag"]
        return SimpleNamespace(status_code=304, headers={}, content=b"")

    result = repo.sync(
        "https://connect.test/assets/index/stable",
        transport=transport,
        policy={"mode": "full", "verify_signatures": True},
    )

    assert result["synced"] is False
    assert result["not_modified"] is True
    assert repo.index.etag == index["etag"]


# Collection imports must not leak the repository ``connect`` namespace into
# later controller tests, which expect controller/connect.py instead.
sys.path[:] = _ORIGINAL_SYS_PATH
for _name in list(sys.modules):
    if _name == "connect" or _name.startswith("connect."):
        sys.modules.pop(_name, None)
