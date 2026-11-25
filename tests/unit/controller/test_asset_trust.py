# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Tests for display asset trust stores, signatures, and the key CLI."""

from __future__ import annotations

import base64
import copy
import json
import stat
import sys
from datetime import datetime, timedelta, timezone
from pathlib import Path
from types import SimpleNamespace

import pytest
from sqlalchemy import event

_REPO_ROOT = Path(__file__).resolve().parents[3]
_CONTROLLER = _REPO_ROOT / "controller"
_SCRIPTS = _REPO_ROOT / "scripts"
_TEST_DIR = Path(__file__).resolve().parent

# The root conftest puts controller/ first, where controller/connect.py would
# shadow the repository's ``connect`` namespace package.  Import Connect first,
# then add controller/ and scripts/ back for the local modules.
_ORIGINAL_SYS_PATH = list(sys.path)
sys.path[:] = [
    entry
    for entry in sys.path
    if entry
    and Path(entry).resolve() != _TEST_DIR
    and not (Path(entry).resolve() / "connect.py").exists()
]
if str(_REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(_REPO_ROOT))
for _name in list(sys.modules):
    if _name == "connect" or _name.startswith("connect."):
        sys.modules.pop(_name, None)

from connect.assets.index import (  # noqa: E402
    AssetService,
    Ed25519Signer,
    HMACSigner,
    build_index,
    canonical_json as connect_canonical_json,
    index_bytes,
)
from connect.assets.models import (  # noqa: E402
    ASSET_SCHEMA,
    create_schema,
    create_sqlite_engine,
)
from connect.assets.storage import MemoryStore  # noqa: E402

if str(_CONTROLLER) not in sys.path:
    sys.path.insert(0, str(_CONTROLLER))
if str(_SCRIPTS) not in sys.path:
    sys.path.insert(0, str(_SCRIPTS))

import display_keys  # noqa: E402
from asset_repository import (  # noqa: E402
    AssetRepository,
    AssetSignatureError,
)
from asset_trust import (  # noqa: E402
    ENV_VAR,
    HAVE_ED25519,
    TrustError,
    TrustStore,
    TrustedKey,
    build_verifier,
    canonical_index_payload,
    canonical_json,
    canonical_manifest_payload,
    key_fingerprint,
    sha256_hex,
)

sys.path[:] = _ORIGINAL_SYS_PATH

pytestmark = pytest.mark.unit
requires_ed25519 = pytest.mark.skipif(
    not HAVE_ED25519, reason="cryptography is not installed"
)

_HMAC_KEY = b"unit-test-signing-key"


# ── Helpers ─────────────────────────────────────────────────────────────────


def _b64(value: bytes) -> str:
    return base64.b64encode(value).decode("ascii")


def _private_key():
    from cryptography.hazmat.primitives.asymmetric.ed25519 import (
        Ed25519PrivateKey,
    )

    return Ed25519PrivateKey.generate()


def _public_b64(private_key) -> str:
    from cryptography.hazmat.primitives import serialization

    return _b64(
        private_key.public_key().public_bytes(
            encoding=serialization.Encoding.Raw,
            format=serialization.PublicFormat.Raw,
        )
    )


def _ed25519_key(private_key, key_id: str = "test-key", **kwargs) -> TrustedKey:
    return TrustedKey(
        key_id=key_id,
        algorithm="ed25519",
        public_key=_public_b64(private_key),
        **kwargs,
    )


def _hmac_key(key_id: str = "hmac-key", **kwargs) -> TrustedKey:
    return TrustedKey(
        key_id=key_id,
        algorithm="hmac-sha256",
        public_key=_HMAC_KEY.hex(),
        **kwargs,
    )


def _sign(private_key, payload: bytes) -> str:
    return _b64(private_key.sign(payload))


def _envelope(algorithm: str, key_id: str, value: str) -> dict:
    return {"algorithm": algorithm, "key_id": key_id, "value": value}


def _sign_index(private_key, index: dict, key_id: str = "test-key") -> dict:
    signed = dict(index)
    signed["signature"] = _envelope(
        "ed25519", key_id, _sign(private_key, canonical_index_payload(index))
    )
    return signed


def _sign_manifest(private_key, manifest: dict, key_id: str = "test-key") -> dict:
    signed = dict(manifest)
    signed["signature"] = _envelope(
        "ed25519",
        key_id,
        _sign(private_key, canonical_manifest_payload(manifest)),
    )
    return signed


def _unsigned_index(entries: list[dict] | None = None) -> dict:
    entries = entries or []
    content_hash = sha256_hex(canonical_json(entries))
    return {
        "schema": "org.ozma.asset-index/1",
        "channel": "stable",
        "generated_at": "2026-01-01T00:00:00+00:00",
        "content_hash": content_hash,
        "etag": f'"{content_hash}"',
        "entries": entries,
    }


def _manifest() -> dict:
    return {
        "id": "org.ozma.widgets.trust-fixture",
        "type": "widget_pack",
        "name": "Trust Fixture",
        "version": "1.2.3",
        "schema": ASSET_SCHEMA,
        "author": "Ozma",
        "description": "Trust-store fixture",
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
        "published_at": "2026-01-01T00:00:00+00:00",
    }


@pytest.fixture
def engine(tmp_path):
    engine = create_sqlite_engine(tmp_path / "assets.db")

    @event.listens_for(engine, "connect")
    def _foreign_keys(connection, _record):
        cursor = connection.cursor()
        cursor.execute("PRAGMA foreign_keys=ON")
        cursor.close()

    create_schema(engine)
    yield engine
    engine.dispose()


def _connect_index(engine, signer) -> dict:
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


def _manifest_files() -> tuple[dict, dict[str, bytes]]:
    data = b"<svg>gauge</svg>"
    digest = sha256_hex(data)
    manifest = {
        "id": "org.ozma.widgets.manifest-fixture",
        "type": "widget_pack",
        "name": "Manifest Fixture",
        "version": "1.0.0",
        "schema": ASSET_SCHEMA,
        "license": "MIT",
        "assets": [
            {"path": "assets/gauge.svg", "sha256": digest, "size": len(data)}
        ],
        "checksums": {
            "algorithm": "sha256",
            "files": {"assets/gauge.svg": digest},
            "signature": "ignored-by-canonicalisation",
        },
    }
    return manifest, {"assets/gauge.svg": data}


# ── Trust store ─────────────────────────────────────────────────────────────


@requires_ed25519
def test_trusted_key_fingerprint_and_repr_do_not_leak_material():
    private_key = _private_key()
    key = _ed25519_key(private_key)
    fingerprint = key_fingerprint(key)

    assert fingerprint == key.fingerprint()
    assert fingerprint.startswith("ed25519:")
    assert _public_b64(private_key) not in fingerprint
    assert _public_b64(private_key) not in repr(key)
    assert _public_b64(private_key) not in repr(TrustStore([key]))
    assert fingerprint != key_fingerprint(_ed25519_key(_private_key()))


@requires_ed25519
def test_from_mapping_accepts_supported_shapes_and_env(monkeypatch):
    private_key = _private_key()
    key = _ed25519_key(private_key)
    document = {
        "key_id": key.key_id,
        "algorithm": key.algorithm,
        "public_key": key.public_key,
    }

    shapes = [
        document,
        [document],
        {"keys": [document]},
        {"keys": {key.key_id: document}},
        {key.key_id: document},
        {"default_key_id": key.key_id, "keys": [document]},
    ]
    for shape in shapes:
        store = TrustStore.from_mapping(shape)
        assert store.get(key.key_id) is not None
        assert store.fingerprint(key.key_id) == key.fingerprint()

    monkeypatch.setenv(ENV_VAR, json.dumps({"keys": [document]}))
    from_env = TrustStore.from_env()
    assert from_env.fingerprint(key.key_id) == key.fingerprint()

    monkeypatch.delenv(ENV_VAR)
    with pytest.raises(TrustError):
        TrustStore.from_env()

    monkeypatch.setenv(ENV_VAR, "{not json")
    with pytest.raises(TrustError):
        TrustStore.from_env()


def test_malformed_key_material_is_rejected():
    with pytest.raises(TrustError):
        TrustedKey("bad", "ed25519", "not base64 !!!")
    with pytest.raises(TrustError):
        TrustedKey("short", "ed25519", _b64(b"short"))
    with pytest.raises(TrustError):
        TrustedKey("empty", "hmac-sha256", "")
    with pytest.raises(TrustError):
        TrustedKey("alg", "rsa", _b64(b"x" * 32))
    with pytest.raises(TrustError):
        TrustedKey(
            "time",
            "hmac-sha256",
            _b64(b"x" * 32),
            not_before="2030-01-01T00:00:00+00:00",
            not_after="2020-01-01T00:00:00+00:00",
        )


@requires_ed25519
def test_duplicate_and_unknown_keys_are_rejected():
    key = _ed25519_key(_private_key())
    store = TrustStore([key])
    with pytest.raises(TrustError):
        store.add(key)
    with pytest.raises(TrustError):
        store.require("missing")
    with pytest.raises(TrustError):
        store.verifier_for("missing")


# ── Index verification ──────────────────────────────────────────────────────


@requires_ed25519
def test_ed25519_index_round_trip_matches_connect_canonical_payload(engine):
    private_key = _private_key()
    signer = Ed25519Signer(private_key, key_id="connect-ed25519")
    index = _connect_index(engine, signer)
    store = TrustStore(
        [
            TrustedKey(
                key_id="connect-ed25519",
                algorithm="ed25519",
                public_key=_b64(signer.public_key_bytes()),
            )
        ]
    )

    unsigned = {key: value for key, value in index.items() if key != "signature"}
    assert canonical_index_payload(index) == connect_canonical_json(unsigned)
    assert store.verify_index(index) is True
    assert store.verifier_for("connect-ed25519").verify(
        canonical_index_payload(index), index["signature"]["value"]
    )


def test_hmac_index_round_trip_matches_connect_canonical_payload(engine):
    signer = HMACSigner(_HMAC_KEY, key_id="connect-hmac")
    index = _connect_index(engine, signer)
    store = TrustStore([_hmac_key("connect-hmac")])

    unsigned = {key: value for key, value in index.items() if key != "signature"}
    assert canonical_index_payload(index) == connect_canonical_json(unsigned)
    assert store.verify_index(index) is True


@requires_ed25519
def test_tampered_index_entries_hash_and_signature_are_rejected(engine):
    private_key = _private_key()
    index = _connect_index(
        engine, Ed25519Signer(private_key, key_id="test-key")
    )
    store = TrustStore([_ed25519_key(private_key)])

    tampered_entry = copy.deepcopy(index)
    tampered_entry["entries"][0]["name"] = "Evil Gauge"
    assert store.verify_index(tampered_entry) is False

    tampered_hash = copy.deepcopy(index)
    tampered_hash["content_hash"] = "0" * 64
    assert store.verify_index(tampered_hash) is False

    tampered_etag = copy.deepcopy(index)
    tampered_etag["etag"] = '"' + "0" * 64 + '"'
    assert store.verify_index(tampered_etag) is False

    tampered_signature = copy.deepcopy(index)
    tampered_signature["signature"]["value"] = _b64(b"\x00" * 64)
    assert store.verify_index(tampered_signature) is False


@requires_ed25519
def test_unknown_revoked_expired_and_not_yet_valid_keys_are_rejected():
    private_key = _private_key()
    index = _sign_index(private_key, _unsigned_index())

    assert TrustStore([_ed25519_key(_private_key())]).verify_index(index) is False
    assert TrustStore([_ed25519_key(private_key, revoked=True)]).verify_index(
        index
    ) is False

    past = (datetime.now(timezone.utc) - timedelta(days=1)).isoformat()
    future = (datetime.now(timezone.utc) + timedelta(days=1)).isoformat()
    assert TrustStore([_ed25519_key(private_key, not_after=past)]).verify_index(
        index
    ) is False
    assert TrustStore(
        [_ed25519_key(private_key, not_before=future)]
    ).verify_index(index) is False
    assert TrustStore(
        [_ed25519_key(private_key, not_before=past, not_after=future)]
    ).verify_index(index) is True


@requires_ed25519
def test_wrong_algorithm_and_malformed_signatures_are_rejected():
    private_key = _private_key()
    index = _sign_index(private_key, _unsigned_index())
    ed25519_store = TrustStore([_ed25519_key(private_key)])
    hmac_store = TrustStore([_hmac_key("test-key")])
    payload = canonical_index_payload(index)

    wrong_algorithm = copy.deepcopy(index)
    wrong_algorithm["signature"]["algorithm"] = "hmac-sha256"
    assert ed25519_store.verify_index(wrong_algorithm) is False
    assert hmac_store.verify_index(index) is False

    for bad_value in ("", "not-base64!!!", _b64(b"short"), _b64(b"\x00" * 63)):
        bad = copy.deepcopy(index)
        bad["signature"]["value"] = bad_value
        assert ed25519_store.verify_index(bad) is False
    assert ed25519_store.verify_index(
        {**index, "signature": {"algorithm": "ed25519", "key_id": "test-key"}}
    ) is False
    assert ed25519_store.verify_index(
        {**index, "signature": 42}
    ) is False
    assert ed25519_store.verifier_for("test-key").verify(
        payload, "not-base64!!!"
    ) is False
    assert ed25519_store.verifier_for("test-key").verify(payload, "") is False


@requires_ed25519
def test_bare_signature_requires_an_explicit_default_key():
    private_key = _private_key()
    index = _sign_index(private_key, _unsigned_index())
    key = _ed25519_key(private_key)
    bare = {
        "schema": index["schema"],
        "channel": index["channel"],
        "generated_at": index["generated_at"],
        "content_hash": index["content_hash"],
        "etag": index["etag"],
        "entries": index["entries"],
        "signature": index["signature"]["value"],
    }

    assert TrustStore([key]).verify_index(bare) is False
    assert TrustStore([key], default_key_id=key.key_id).verify_index(bare) is True


# ── Manifest verification ───────────────────────────────────────────────────


@requires_ed25519
def test_manifest_round_trip_and_optional_file_hashes():
    private_key = _private_key()
    manifest, files = _manifest_files()
    store = TrustStore([_ed25519_key(private_key)])
    signed = _sign_manifest(private_key, manifest)

    assert store.verify_manifest(signed) is True
    assert store.verify_manifest(signed, files) is True
    assert store.verify_manifest(
        signed, {"assets/gauge.svg": sha256_hex(files["assets/gauge.svg"])}
    ) is True

    bad_manifest = dict(signed)
    bad_manifest["name"] = "Tampered"
    assert store.verify_manifest(bad_manifest, files) is False

    tampered_files = {"assets/gauge.svg": b"<svg>tampered</svg>"}
    assert store.verify_manifest(signed, tampered_files) is False
    assert store.verify_manifest(signed, {}) is False

    bad_signature = copy.deepcopy(signed)
    bad_signature["signature"]["value"] = _b64(b"\x00" * 64)
    assert store.verify_manifest(bad_signature) is False

    nested = copy.deepcopy(manifest)
    nested["checksums"]["signature"] = _envelope(
        "ed25519", "test-key", _sign(private_key, canonical_manifest_payload(nested))
    )
    assert "signature" not in nested
    assert store.verify_manifest(nested, files) is True


@requires_ed25519
def test_manifest_canonicalisation_excludes_signatures_but_keeps_files():
    manifest, _files = _manifest_files()
    first = canonical_manifest_payload(manifest)
    changed_signature = copy.deepcopy(manifest)
    changed_signature["signature"] = {"algorithm": "ed25519", "value": "ignored"}
    changed_signature["checksums"]["signature"] = "also-ignored"
    assert canonical_manifest_payload(changed_signature) == first

    changed_files = copy.deepcopy(manifest)
    changed_files["checksums"]["files"]["assets/gauge.svg"] = "c" * 64
    assert canonical_manifest_payload(changed_files) != first


@requires_ed25519
def test_manifest_conflicts_unsafe_paths_and_bad_digests_fail_closed():
    private_key = _private_key()
    manifest, files = _manifest_files()
    store = TrustStore([_ed25519_key(private_key)])

    conflicting = copy.deepcopy(manifest)
    conflicting["assets"][0]["sha256"] = "d" * 64
    signed_conflict = _sign_manifest(private_key, conflicting)
    assert store.verify_manifest(signed_conflict, files) is False

    unsafe = copy.deepcopy(manifest)
    unsafe["checksums"]["files"] = {"../escape": sha256_hex(b"x")}
    signed_unsafe = _sign_manifest(private_key, unsafe)
    assert store.verify_manifest(signed_unsafe, files) is False

    bad_digest = copy.deepcopy(manifest)
    bad_digest["checksums"]["files"] = {"assets/gauge.svg": "not-a-digest"}
    signed_digest = _sign_manifest(private_key, bad_digest)
    assert store.verify_manifest(signed_digest, files) is False

    flat = copy.deepcopy(manifest)
    flat["checksums"] = {"assets/gauge.svg": sha256_hex(files["assets/gauge.svg"])}
    signed_flat = _sign_manifest(private_key, flat)
    assert store.verify_manifest(signed_flat, files) is True
    assert store.verify_manifest(
        signed_flat, {"assets/gauge.svg": b"tampered"}
    ) is False


# ── Repository adapter ──────────────────────────────────────────────────────


@requires_ed25519
def test_build_verifier_uses_one_unambiguous_key():
    private_key = _private_key()
    key = _ed25519_key(private_key)
    verifier = build_verifier(TrustStore([key]))
    index = _sign_index(private_key, _unsigned_index())
    payload = canonical_index_payload(index)

    assert verifier(payload, index["signature"]["value"], {}) is True
    assert verifier(b"not json", index["signature"]["value"], {}) is False
    assert verifier(payload, "wrong", {}) is False

    other = _ed25519_key(_private_key(), key_id="other")
    with pytest.raises(TrustError):
        build_verifier(TrustStore([key, other]))
    with pytest.raises(TrustError):
        build_verifier(TrustStore())
    assert build_verifier(
        TrustStore([key, other], default_key_id="other")
    )(payload, index["signature"]["value"], {}) is False


@requires_ed25519
def test_build_verifier_verifies_manifest_payloads_with_files():
    private_key = _private_key()
    store = TrustStore([_ed25519_key(private_key)])
    verifier = build_verifier(store)
    manifest, files = _manifest_files()
    signed = _sign_manifest(private_key, manifest)
    raw = json.dumps(signed).encode("utf-8")

    assert verifier(raw, signed["signature"]["value"], files) is True
    assert verifier(
        raw,
        signed["signature"]["value"],
        {"assets/gauge.svg": b"tampered"},
    ) is False


@requires_ed25519
def test_build_verifier_works_with_asset_repository_sync(engine, tmp_path):
    private_key = _private_key()
    signer = Ed25519Signer(private_key, key_id="connect-ed25519")
    index = _connect_index(engine, signer)
    store = TrustStore(
        [
            TrustedKey(
                key_id="connect-ed25519",
                algorithm="ed25519",
                public_key=_b64(signer.public_key_bytes()),
            )
        ]
    )
    repository = AssetRepository(
        packs_dir=tmp_path / "packs",
        cache_dir=tmp_path / "cache",
        allow_unsigned=False,
        signature_verifier=build_verifier(store),
    )
    body = index_bytes(index)

    def transport(url, headers=None):
        return SimpleNamespace(
            status_code=200,
            headers={"ETag": index["etag"]},
            content=body,
        )

    result = repository.sync(
        "https://connect.test/assets/index/stable",
        transport=transport,
        policy={"mode": "full", "verify_signatures": True},
    )
    assert result["synced"] is True
    assert repository.index.entry("org.ozma.widgets.trust-fixture") is not None
    assert repository.index.signature_meta["key_id"] == "connect-ed25519"

    tampered = copy.deepcopy(index)
    tampered["entries"][0]["name"] = "Evil Gauge"
    tampered_body = index_bytes(tampered)

    def tampered_transport(url, headers=None):
        return SimpleNamespace(
            status_code=200,
            headers={"ETag": index["etag"]},
            content=tampered_body,
        )

    with pytest.raises(AssetSignatureError):
        repository.sync(
            "https://connect.test/assets/index/stable",
            transport=tampered_transport,
            policy={"mode": "full", "verify_signatures": True},
        )


# ── CLI ─────────────────────────────────────────────────────────────────────


def _generate(tmp_path: Path, key_id: str = "cli-key", force: bool = True):
    private_path = tmp_path / "private.pem"
    public_path = tmp_path / "public.json"
    arguments = [
        "generate",
        "--key-id",
        key_id,
        "--private-out",
        str(private_path),
        "--public-out",
        str(public_path),
    ]
    if force:
        arguments.append("--force")
    assert display_keys.main(arguments) == 0
    return private_path, public_path, json.loads(public_path.read_text())


@requires_ed25519
def test_cli_generate_writes_0600_private_key_and_prints_only_fingerprint(
    tmp_path, capsys
):
    private_path, public_path, document = _generate(tmp_path)

    assert stat.S_IMODE(private_path.stat().st_mode) == 0o600
    assert document["algorithm"] == "ed25519"
    assert document["key_id"] == "cli-key"
    assert b"PRIVATE KEY" in private_path.read_bytes()

    captured = capsys.readouterr()
    expected = TrustStore.from_mapping(document).fingerprint("cli-key")
    assert captured.out == expected + "\n"
    assert "PRIVATE" not in captured.out
    assert "PRIVATE" not in captured.err
    assert private_path.read_text() not in captured.out + captured.err

    assert display_keys.main(
        ["fingerprint", "--public", str(public_path)]
    ) == 0
    assert capsys.readouterr().out == expected + "\n"
    assert display_keys.main(
        [
            "fingerprint",
            "--key-id",
            "cli-key",
            "--trust",
            str(public_path),
        ]
    ) == 0
    assert capsys.readouterr().out == expected + "\n"


@requires_ed25519
def test_cli_generate_refuses_overwrite_without_force(tmp_path, capsys):
    private_path, public_path, _document = _generate(tmp_path)
    original = private_path.read_bytes()

    arguments = [
        "generate",
        "--key-id",
        "cli-key",
        "--private-out",
        str(private_path),
        "--public-out",
        str(public_path),
    ]
    assert display_keys.main(arguments) == 1
    captured = capsys.readouterr()
    assert "refusing to overwrite" in json.loads(captured.err)["error"]
    assert private_path.read_bytes() == original

    assert display_keys.main(arguments + ["--force"]) == 0
    assert private_path.read_bytes() != original
    assert stat.S_IMODE(private_path.stat().st_mode) == 0o600
    assert capsys.readouterr().out.startswith("ed25519:")


@requires_ed25519
def test_cli_sign_and_verify_index(tmp_path, capsys, monkeypatch):
    private_path, public_path, _document = _generate(tmp_path)
    capsys.readouterr()
    index_path = tmp_path / "index.json"
    signed_path = tmp_path / "index.signed.json"
    index_path.write_text(json.dumps(_unsigned_index()))

    assert display_keys.main(
        [
            "sign-index",
            "--private",
            str(private_path),
            "--key-id",
            "cli-key",
            "--in",
            str(index_path),
            "--out",
            str(signed_path),
        ]
    ) == 0
    assert json.loads(capsys.readouterr().out)["signed"] is True
    signed = json.loads(signed_path.read_text())
    assert signed["signature"]["key_id"] == "cli-key"
    assert signed["signature"]["algorithm"] == "ed25519"

    assert display_keys.main(
        [
            "verify-index",
            "--trust",
            str(public_path),
            "--in",
            str(signed_path),
        ]
    ) == 0
    result = json.loads(capsys.readouterr().out)
    assert result["verified"] is True
    assert result["key_id"] == "cli-key"

    monkeypatch.setenv(ENV_VAR, public_path.read_text())
    assert display_keys.main(
        [
            "verify-index",
            "--trust-env",
            "--in",
            str(signed_path),
        ]
    ) == 0
    assert json.loads(capsys.readouterr().out)["verified"] is True

    tampered_path = tmp_path / "tampered.json"
    tampered = dict(signed)
    tampered["entries"] = [{"id": "evil"}]
    tampered_path.write_text(json.dumps(tampered))
    assert display_keys.main(
        [
            "verify-index",
            "--trust",
            str(public_path),
            "--in",
            str(tampered_path),
        ]
    ) == 1
    assert json.loads(capsys.readouterr().out)["verified"] is False

    assert display_keys.main(
        [
            "verify-index",
            "--in",
            str(signed_path),
        ]
    ) == 1
    assert "exactly one of --trust or --trust-env" in json.loads(
        capsys.readouterr().err
    )["error"]


@requires_ed25519
def test_cli_sign_and_verify_manifest_with_file_hashes(tmp_path, capsys):
    private_path, public_path, _document = _generate(tmp_path)
    capsys.readouterr()
    manifest, files = _manifest_files()
    root = tmp_path / "pack"
    (root / "assets").mkdir(parents=True)
    for name, data in files.items():
        (root / name).write_bytes(data)

    manifest_path = tmp_path / "manifest.json"
    signed_path = tmp_path / "manifest.signed.json"
    manifest_path.write_text(json.dumps(manifest))

    assert display_keys.main(
        [
            "sign-manifest",
            "--private",
            str(private_path),
            "--key-id",
            "cli-key",
            "--in",
            str(manifest_path),
            "--out",
            str(signed_path),
        ]
    ) == 0
    assert json.loads(capsys.readouterr().out)["kind"] == "manifest"

    assert display_keys.main(
        [
            "verify-manifest",
            "--trust",
            str(public_path),
            "--in",
            str(signed_path),
            "--root",
            str(root),
        ]
    ) == 0
    result = json.loads(capsys.readouterr().out)
    assert result["verified"] is True
    assert result["files_checked"] is True

    (root / "assets" / "gauge.svg").write_bytes(b"tampered")
    assert display_keys.main(
        [
            "verify-manifest",
            "--trust",
            str(public_path),
            "--in",
            str(signed_path),
            "--root",
            str(root),
        ]
    ) == 1
    assert json.loads(capsys.readouterr().out)["verified"] is False


@requires_ed25519
def test_cli_errors_are_json_and_never_print_private_material(tmp_path, capsys):
    private_path, public_path, _document = _generate(tmp_path)
    private_text = private_path.read_text()

    assert display_keys.main(
        [
            "verify-index",
            "--trust",
            str(public_path),
            "--in",
            str(tmp_path / "missing.json"),
        ]
    ) == 1
    payload = json.loads(capsys.readouterr().err)
    assert "cannot read index" in payload["error"]
    assert private_text not in payload["error"]

    assert display_keys.main(
        [
            "sign-index",
            "--private",
            str(tmp_path / "missing.pem"),
            "--key-id",
            "cli-key",
            "--in",
            str(tmp_path / "missing.json"),
            "--out",
            str(tmp_path / "out.json"),
        ]
    ) == 1
    assert "cannot read private key" in json.loads(capsys.readouterr().err)["error"]
    assert private_text not in capsys.readouterr().out


@requires_ed25519
def test_cli_rejects_missing_trust_and_unknown_key(tmp_path, capsys):
    _private_path, public_path, _document = _generate(tmp_path)
    index_path = tmp_path / "index.json"
    signed_path = tmp_path / "signed.json"
    index_path.write_text(json.dumps(_unsigned_index()))
    display_keys.main(
        [
            "sign-index",
            "--private",
            str(tmp_path / "private.pem"),
            "--key-id",
            "cli-key",
            "--in",
            str(index_path),
            "--out",
            str(signed_path),
        ]
    )

    assert display_keys.main(
        [
            "fingerprint",
            "--key-id",
            "does-not-exist",
            "--trust",
            str(public_path),
        ]
    ) == 1
    assert "unknown signing key" in json.loads(capsys.readouterr().err)["error"]
