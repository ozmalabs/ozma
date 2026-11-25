# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for the display-platform configuration surface."""

from __future__ import annotations

import pytest

from display_config import (
    DEFAULT_MAX_IMPORT_BYTES,
    DEFAULT_MAX_PACK_BYTES,
    DEFAULT_PROFILE,
    DisplayConfig,
    DisplayConfigError,
)

pytestmark = pytest.mark.unit

_ENV_KEYS = (
    "OZMA_DISPLAY_PROFILE",
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


def _clear_display_env(monkeypatch: pytest.MonkeyPatch) -> None:
    for key in _ENV_KEYS:
        monkeypatch.delenv(key, raising=False)


def test_defaults_use_the_local_profile(monkeypatch):
    _clear_display_env(monkeypatch)
    config = DisplayConfig.from_env()

    assert DEFAULT_PROFILE == "local"
    assert config.profile == "local"
    assert config.ir_enabled is True
    assert config.asset_repository_enabled is True
    assert config.sync_mode == "offline"
    assert config.fonts_translation_enabled is True
    assert config.importers_enabled is False
    assert config.asset_index_url == ""
    assert config.cache_dir == ""
    assert config.allow_unsigned_packs is False
    assert config.signature_required is False
    assert config.max_pack_bytes == DEFAULT_MAX_PACK_BYTES
    assert config.max_import_bytes == DEFAULT_MAX_IMPORT_BYTES
    assert config.max_pack_bytes > 0
    assert config.max_import_bytes > 0


@pytest.mark.parametrize(
    ("profile", "ir", "repository", "fonts", "importers"),
    [
        ("off", False, False, False, False),
        ("local", True, True, True, False),
        ("full", True, True, True, True),
    ],
)
def test_profile_matrix(profile, ir, repository, fonts, importers):
    config = DisplayConfig.from_mapping({"profile": profile})

    assert config.profile == profile
    assert config.ir_enabled is ir
    assert config.asset_repository_enabled is repository
    assert config.fonts_translation_enabled is fonts
    assert config.importers_enabled is importers
    # No profile silently enables network sync; ``full`` included.
    assert config.sync_mode == "offline"
    assert config.allow_unsigned_packs is False
    assert config.signature_required is False


def test_off_profile_preserves_legacy_all_off_behaviour(monkeypatch):
    _clear_display_env(monkeypatch)
    monkeypatch.setenv("OZMA_DISPLAY_PROFILE", "off")

    config = DisplayConfig.from_env()

    assert config.profile == "off"
    assert config.ir_enabled is False
    assert config.asset_repository_enabled is False
    assert config.fonts_translation_enabled is False
    assert config.importers_enabled is False
    assert config.sync_mode == "offline"


def test_full_profile_enables_importers_without_enabling_network():
    config = DisplayConfig.from_mapping({"OZMA_DISPLAY_PROFILE": "full"})

    assert config.importers_enabled is True
    assert config.sync_mode == "offline"
    assert config.signature_required is False
    assert config.allow_unsigned_packs is False


def test_explicit_flags_override_the_profile():
    off_with_flags = DisplayConfig.from_mapping({
        "OZMA_DISPLAY_PROFILE": "off",
        "OZMA_DISPLAY_IR_ENABLED": "1",
        "OZMA_DISPLAY_ASSET_REPOSITORY_ENABLED": "yes",
        "OZMA_DISPLAY_FONTS_TRANSLATION_ENABLED": "on",
        "OZMA_DISPLAY_IMPORTERS_ENABLED": "true",
    })
    assert off_with_flags.profile == "off"
    assert off_with_flags.ir_enabled is True
    assert off_with_flags.asset_repository_enabled is True
    assert off_with_flags.fonts_translation_enabled is True
    assert off_with_flags.importers_enabled is True

    local_with_overrides = DisplayConfig.from_mapping({
        "OZMA_DISPLAY_PROFILE": "local",
        "OZMA_DISPLAY_IR_ENABLED": "0",
        "OZMA_DISPLAY_IMPORTERS_ENABLED": "1",
        "OZMA_DISPLAY_SYNC_MODE": "manual",
    })
    assert local_with_overrides.profile == "local"
    assert local_with_overrides.ir_enabled is False
    assert local_with_overrides.asset_repository_enabled is True
    assert local_with_overrides.fonts_translation_enabled is True
    assert local_with_overrides.importers_enabled is True
    assert local_with_overrides.sync_mode == "manual"

    full_with_overrides = DisplayConfig.from_mapping({
        "OZMA_DISPLAY_PROFILE": "full",
        "OZMA_DISPLAY_IMPORTERS_ENABLED": "0",
        "OZMA_DISPLAY_SYNC_MODE": "installed",
    })
    assert full_with_overrides.profile == "full"
    assert full_with_overrides.importers_enabled is False
    assert full_with_overrides.sync_mode == "installed"


def test_profile_never_relaxes_signature_or_unsigned_policy():
    for profile in ("off", "local", "full"):
        config = DisplayConfig.from_mapping({"profile": profile})
        assert config.allow_unsigned_packs is False
        assert config.signature_required is False

    strict = DisplayConfig.from_mapping({
        "profile": "full",
        "OZMA_DISPLAY_ALLOW_UNSIGNED_PACKS": "1",
        "OZMA_DISPLAY_SIGNATURE_REQUIRED": "yes",
    })
    assert strict.allow_unsigned_packs is True
    assert strict.signature_required is True


@pytest.mark.parametrize("raw", ["LOCAL", " local ", "Full", "OFF"])
def test_profile_is_case_insensitive_and_trimmed(raw):
    assert DisplayConfig.from_mapping({"profile": raw}).profile == raw.lower().strip()


@pytest.mark.parametrize(
    "bad_value",
    ["", "all", "on", "enabled", " local-full ", None, 1, [], {}],
)
def test_invalid_profile_raises_typed_error(bad_value):
    with pytest.raises(DisplayConfigError) as excinfo:
        DisplayConfig.from_mapping({"profile": bad_value})
    assert excinfo.value.variable == "OZMA_DISPLAY_PROFILE"


def test_invalid_profile_from_env_raises_typed_error(monkeypatch):
    _clear_display_env(monkeypatch)
    monkeypatch.setenv("OZMA_DISPLAY_PROFILE", "everything")

    with pytest.raises(DisplayConfigError) as excinfo:
        DisplayConfig.from_env()

    assert excinfo.value.variable == "OZMA_DISPLAY_PROFILE"


def test_from_env_reads_every_supported_value(monkeypatch):
    _clear_display_env(monkeypatch)
    monkeypatch.setenv("OZMA_DISPLAY_PROFILE", "full")
    monkeypatch.setenv("OZMA_DISPLAY_IR_ENABLED", "yes")
    monkeypatch.setenv("OZMA_DISPLAY_ASSET_REPOSITORY_ENABLED", "ON")
    monkeypatch.setenv("OZMA_DISPLAY_SYNC_MODE", " Manual ")
    monkeypatch.setenv("OZMA_DISPLAY_FONTS_TRANSLATION_ENABLED", "1")
    monkeypatch.setenv("OZMA_DISPLAY_IMPORTERS_ENABLED", "true")
    monkeypatch.setenv(
        "OZMA_DISPLAY_ASSET_INDEX_URL", "https://assets.example.test/index.json"
    )
    monkeypatch.setenv("OZMA_DISPLAY_CACHE_DIR", "/var/lib/ozma/asset-cache")
    monkeypatch.setenv("OZMA_DISPLAY_ALLOW_UNSIGNED_PACKS", "no")
    monkeypatch.setenv("OZMA_DISPLAY_SIGNATURE_REQUIRED", "yes")
    monkeypatch.setenv("OZMA_DISPLAY_TRUSTED_KEYS", '{"keys": []}')
    monkeypatch.setenv("OZMA_DISPLAY_MAX_PACK_BYTES", "10485760")
    monkeypatch.setenv("OZMA_DISPLAY_MAX_IMPORT_BYTES", "2048")

    config = DisplayConfig.from_env()

    assert config.profile == "full"
    assert config.ir_enabled is True
    assert config.asset_repository_enabled is True
    assert config.sync_mode == "manual"
    assert config.fonts_translation_enabled is True
    assert config.importers_enabled is True
    assert config.asset_index_url == "https://assets.example.test/index.json"
    assert config.cache_dir == "/var/lib/ozma/asset-cache"
    assert config.allow_unsigned_packs is False
    assert config.signature_required is True
    assert config.trusted_keys == '{"keys": []}'
    assert config.max_pack_bytes == 10_485_760
    assert config.max_import_bytes == 2048


def test_from_env_ignores_unrelated_variables(monkeypatch):
    _clear_display_env(monkeypatch)
    monkeypatch.setenv("OZMA_DISPLAY_IR_ENABLED", "on")
    monkeypatch.setenv("OZMA_DISPLAY_UNRELATED", "not-a-real-flag")
    monkeypatch.setenv("PATH", "/usr/bin")

    config = DisplayConfig.from_env()

    assert config.profile == "local"
    assert config.ir_enabled is True
    assert config.sync_mode == "offline"


@pytest.mark.parametrize(
    ("raw", "expected"),
    [
        ("1", True),
        ("0", False),
        ("true", True),
        ("false", False),
        ("TRUE", True),
        ("FALSE", False),
        ("yes", True),
        ("no", False),
        ("on", True),
        ("off", False),
        (" on ", True),
        (" Off ", False),
    ],
)
def test_strict_boolean_spellings(raw, expected):
    config = DisplayConfig.from_mapping({"ir_enabled": raw})
    assert config.ir_enabled is expected


@pytest.mark.parametrize(
    "bad_value",
    ["", "2", "-1", "truthy", "enabled", "nope", None, [], {}],
)
def test_invalid_booleans_raise_typed_error(bad_value):
    with pytest.raises(DisplayConfigError) as excinfo:
        DisplayConfig.from_mapping({"ir_enabled": bad_value})
    assert excinfo.value.variable == "OZMA_DISPLAY_IR_ENABLED"


def test_boolean_accepts_native_bool_and_int():
    assert DisplayConfig.from_mapping({"importers_enabled": True}).importers_enabled
    assert not DisplayConfig.from_mapping(
        {"importers_enabled": 0}
    ).importers_enabled


def test_invalid_sync_mode_raises_typed_error():
    with pytest.raises(DisplayConfigError) as excinfo:
        DisplayConfig.from_mapping({"sync_mode": "sometimes"})
    assert excinfo.value.variable == "OZMA_DISPLAY_SYNC_MODE"


@pytest.mark.parametrize("mode", ["offline", "installed", "manual", "full"])
def test_sync_modes_are_accepted(mode):
    assert DisplayConfig.from_mapping({"sync_mode": mode}).sync_mode == mode


@pytest.mark.parametrize(
    ("field", "variable"),
    [
        ("max_pack_bytes", "OZMA_DISPLAY_MAX_PACK_BYTES"),
        ("max_import_bytes", "OZMA_DISPLAY_MAX_IMPORT_BYTES"),
    ],
)
@pytest.mark.parametrize("bad_value", ["-1", "1.5", "64MB", "", None, True])
def test_invalid_sizes_raise_typed_error(field, variable, bad_value):
    with pytest.raises(DisplayConfigError) as excinfo:
        DisplayConfig.from_mapping({field: bad_value})
    assert excinfo.value.variable == variable


def test_constructor_validates_direct_values():
    with pytest.raises(DisplayConfigError):
        DisplayConfig(sync_mode="scheduled")
    with pytest.raises(DisplayConfigError):
        DisplayConfig(ir_enabled="true")
    with pytest.raises(DisplayConfigError):
        DisplayConfig(ir_enabled=None)
    with pytest.raises(DisplayConfigError):
        DisplayConfig(max_pack_bytes=-1)
    with pytest.raises(DisplayConfigError) as excinfo:
        DisplayConfig(profile="everything")
    assert excinfo.value.variable == "OZMA_DISPLAY_PROFILE"


def test_constructor_applies_profile_defaults_and_explicit_overrides():
    local = DisplayConfig()
    assert local.profile == "local"
    assert local.ir_enabled is True
    assert local.asset_repository_enabled is True
    assert local.fonts_translation_enabled is True
    assert local.importers_enabled is False

    off = DisplayConfig(profile="off")
    assert off.ir_enabled is False
    assert off.asset_repository_enabled is False
    assert off.fonts_translation_enabled is False
    assert off.importers_enabled is False

    overridden = DisplayConfig(profile="local", ir_enabled=False, importers_enabled=True)
    assert overridden.ir_enabled is False
    assert overridden.importers_enabled is True


def test_from_mapping_accepts_env_keys_and_field_names():
    from_env_keys = DisplayConfig.from_mapping({
        "OZMA_DISPLAY_PROFILE": "off",
        "OZMA_DISPLAY_IR_ENABLED": "1",
        "OZMA_DISPLAY_SYNC_MODE": "installed",
    })
    from_fields = DisplayConfig.from_mapping({
        "profile": "off",
        "ir_enabled": "1",
        "sync_mode": "installed",
    })
    assert from_env_keys == from_fields
    assert from_env_keys.profile == "off"
    assert from_env_keys.ir_enabled is True


def test_to_dict_round_trips_through_from_mapping():
    config = DisplayConfig(
        ir_enabled=True,
        asset_repository_enabled=True,
        sync_mode="full",
        fonts_translation_enabled=True,
        importers_enabled=True,
        asset_index_url="https://assets.example.test/index.json",
        cache_dir="/tmp/ozma-cache",
        allow_unsigned_packs=True,
        max_pack_bytes=1234,
        max_import_bytes=5678,
        profile="full",
    )
    assert DisplayConfig.from_mapping(config.to_dict()) == config
    assert config.to_dict()["sync_mode"] == "full"
    assert config.to_dict()["profile"] == "full"


def test_feature_flags_map_to_plan_names():
    config = DisplayConfig(
        ir_enabled=True,
        asset_repository_enabled=True,
        sync_mode="installed",
        fonts_translation_enabled=True,
        importers_enabled=True,
        profile="full",
    )
    assert config.feature_flags() == {
        "display.ir.enabled": True,
        "display.asset_repository.enabled": True,
        "display.asset_sync.mode": "installed",
        "display.fonts.translation": "enabled",
        "display.importers.enabled": True,
    }


def test_default_feature_flags_match_the_local_profile():
    flags = DisplayConfig().feature_flags()
    assert flags["display.ir.enabled"] is True
    assert flags["display.asset_repository.enabled"] is True
    assert flags["display.asset_sync.mode"] == "offline"
    assert flags["display.fonts.translation"] == "enabled"
    assert flags["display.importers.enabled"] is False


def test_off_profile_feature_flags_are_disabled():
    flags = DisplayConfig(profile="off").feature_flags()
    assert flags["display.ir.enabled"] is False
    assert flags["display.asset_repository.enabled"] is False
    assert flags["display.asset_sync.mode"] == "offline"
    assert flags["display.fonts.translation"] == "disabled"
    assert flags["display.importers.enabled"] is False
