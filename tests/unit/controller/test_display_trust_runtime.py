# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Trust-store wiring into DisplayRuntime."""

from __future__ import annotations

import hashlib
import hmac
import json
from pathlib import Path

import pytest

from display_config import DisplayConfig, DisplayConfigError
from display_runtime import build_runtime

_KEY = bytes.fromhex("11" * 32)
_INDEX_SCHEMA = "org.ozma.asset-index/1"


def _trust_json(*, default: str | None = "test", keys: list[dict] | None = None) -> str:
    payload = {
        "keys": keys
        or [
            {
                "key_id": "test",
                "algorithm": "hmac-sha256",
                "public_key": "hex:" + _KEY.hex(),
            }
        ]
    }
    if default:
        payload["default_key_id"] = default
    return json.dumps(payload)


def _canonical(value: object) -> bytes:
    return json.dumps(
        value, sort_keys=True, separators=(",", ":"), ensure_ascii=False
    ).encode("utf-8")


def _sign(payload: bytes) -> str:
    return hmac.new(_KEY, payload, hashlib.sha256).hexdigest()


def test_runtime_builds_verifier_from_trusted_keys(tmp_path: Path) -> None:
    runtime = build_runtime(
        {"trusted_keys": _trust_json(), "signature_required": True},
        cache_dir=tmp_path / "cache",
    )
    verifier = runtime.asset_repository.signature_verifier
    assert verifier is not None
    assert runtime.asset_repository.require_signature is True

    index = {
        "schema": _INDEX_SCHEMA,
        "channel": "stable",
        "entries": [],
        "content_hash": "a" * 64,
    }
    payload = _canonical(index)
    signature = _sign(payload)
    assert verifier(payload, signature, {}) is True
    assert verifier(payload, "0" * 64, {}) is False
    assert verifier(payload.replace(b"stable", b"tampered"), signature, {}) is False


def test_runtime_rejects_invalid_trust_json(tmp_path: Path) -> None:
    with pytest.raises(DisplayConfigError) as caught:
        build_runtime(
            {"trusted_keys": "{not json"},
            cache_dir=tmp_path / "cache",
        )
    assert caught.value.variable == "OZMA_DISPLAY_TRUSTED_KEYS"


def test_runtime_rejects_ambiguous_trust_store(tmp_path: Path) -> None:
    keys = [
        {"key_id": "a", "algorithm": "hmac-sha256", "public_key": "hex:" + _KEY.hex()},
        {"key_id": "b", "algorithm": "hmac-sha256", "public_key": "hex:" + _KEY.hex()},
    ]
    with pytest.raises(DisplayConfigError) as caught:
        build_runtime(
            {"trusted_keys": _trust_json(default=None, keys=keys)},
            cache_dir=tmp_path / "cache",
        )
    assert "multiple signing keys" in str(caught.value)


def test_runtime_without_trust_keys_has_no_verifier(tmp_path: Path) -> None:
    runtime = build_runtime({}, cache_dir=tmp_path / "cache")
    assert runtime.asset_repository.signature_verifier is None
    assert runtime.config.signature_required is False


def test_status_reports_key_presence_without_material(tmp_path: Path) -> None:
    runtime = build_runtime(
        {"trusted_keys": _trust_json()},
        cache_dir=tmp_path / "cache",
    )
    status = runtime.status()
    text = json.dumps(status, sort_keys=True)
    assert status["trusted_keys_configured"] is True
    assert "hmac-sha256" not in text
    assert _KEY.hex() not in text
    assert "test" not in text


def test_injected_verifier_wins_over_config(tmp_path: Path) -> None:
    sentinel = lambda payload, signature, files: True  # noqa: E731
    runtime = build_runtime(
        DisplayConfig.from_mapping({"trusted_keys": _trust_json()}),
        cache_dir=tmp_path / "cache",
        signature_verifier=sentinel,
    )
    assert runtime.asset_repository.signature_verifier is sentinel
