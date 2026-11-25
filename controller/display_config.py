# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Configuration surface for the display platform.

The display platform is controlled by a small rollout profile plus the
individual ``OZMA_DISPLAY_*`` feature flags.  This module is the single place
that reads those environment variables, so the controller, scripts, and tests
agree on defaults and on what counts as a valid value.

Design rules:

* no network, no filesystem writes, and no import-time side effects;
* strict booleans (``1/0``, ``true/false``, ``yes/no``, ``on/off``);
* typed errors instead of silently falling back to a default;
* explicit flags override the selected profile;
* every size limit is explicit and conservative.

The default profile is ``local``: lossless IR, font translation, and the local
asset repository are on, while private importers and network sync remain off.
``full`` additionally enables importers but still leaves sync at ``offline``
until the operator explicitly selects a network mode.  ``off`` preserves the
legacy all-off behaviour.

See ``docs/display-platform/README.md`` and
``docs/display-platform-execution-plan.md`` (M7, §13) for the rollout flags.
"""

from __future__ import annotations

import os
from collections.abc import Mapping
from dataclasses import asdict, dataclass
from typing import Any

ENV_PREFIX = "OZMA_DISPLAY_"

PROFILES = frozenset({"off", "local", "full"})
DEFAULT_PROFILE = "local"

SYNC_MODES = frozenset({"offline", "installed", "manual", "full"})

DEFAULT_MAX_PACK_BYTES = 64 * 1024 * 1024
DEFAULT_MAX_IMPORT_BYTES = 32 * 1024 * 1024

_TRUE_VALUES = frozenset({"1", "true", "yes", "on"})
_FALSE_VALUES = frozenset({"0", "false", "no", "off"})

#: Internal sentinel for feature flags whose value comes from the profile.
#: Unlike ``None`` it can never be supplied as a deliberate flag value.
_UNSET = object()

_ENV_TO_FIELD = {
    "OZMA_DISPLAY_PROFILE": "profile",
    "OZMA_DISPLAY_IR_ENABLED": "ir_enabled",
    "OZMA_DISPLAY_ASSET_REPOSITORY_ENABLED": "asset_repository_enabled",
    "OZMA_DISPLAY_SYNC_MODE": "sync_mode",
    "OZMA_DISPLAY_FONTS_TRANSLATION_ENABLED": "fonts_translation_enabled",
    "OZMA_DISPLAY_IMPORTERS_ENABLED": "importers_enabled",
    "OZMA_DISPLAY_ASSET_INDEX_URL": "asset_index_url",
    "OZMA_DISPLAY_CACHE_DIR": "cache_dir",
    "OZMA_DISPLAY_ALLOW_UNSIGNED_PACKS": "allow_unsigned_packs",
    "OZMA_DISPLAY_SIGNATURE_REQUIRED": "signature_required",
    "OZMA_DISPLAY_TRUSTED_KEYS": "trusted_keys",
    "OZMA_DISPLAY_MAX_PACK_BYTES": "max_pack_bytes",
    "OZMA_DISPLAY_MAX_IMPORT_BYTES": "max_import_bytes",
}

_FIELD_NAMES = frozenset(_ENV_TO_FIELD.values())

#: Bool fields whose unset value follows the selected profile.  The signature
#: and unsigned-pack policy fields are deliberately excluded: a profile never
#: relaxes or tightens them.
_PROFILE_FEATURE_FIELDS = (
    "ir_enabled",
    "asset_repository_enabled",
    "fonts_translation_enabled",
    "importers_enabled",
)

_PROFILE_DEFAULTS: dict[str, dict[str, Any]] = {
    "off": {
        "ir_enabled": False,
        "asset_repository_enabled": False,
        "fonts_translation_enabled": False,
        "importers_enabled": False,
        "sync_mode": "offline",
    },
    "local": {
        "ir_enabled": True,
        "asset_repository_enabled": True,
        "fonts_translation_enabled": True,
        "importers_enabled": False,
        "sync_mode": "offline",
    },
    "full": {
        "ir_enabled": True,
        "asset_repository_enabled": True,
        "fonts_translation_enabled": True,
        "importers_enabled": True,
        "sync_mode": "offline",
    },
}

_BOOL_FIELDS = (
    "ir_enabled",
    "asset_repository_enabled",
    "fonts_translation_enabled",
    "importers_enabled",
    "allow_unsigned_packs",
    "signature_required",
)

_SIZE_FIELDS = ("max_pack_bytes", "max_import_bytes")

_STRING_FIELDS = ("asset_index_url", "cache_dir", "trusted_keys")


class DisplayConfigError(ValueError):
    """Raised when a display configuration value is missing or malformed."""

    def __init__(self, message: str, *, variable: str = "") -> None:
        super().__init__(message)
        self.variable = variable


def _parse_bool(value: Any, variable: str) -> bool:
    if isinstance(value, bool):
        return value
    if isinstance(value, int) and value in (0, 1):
        return bool(value)
    if isinstance(value, str):
        token = value.strip().lower()
        if token in _TRUE_VALUES:
            return True
        if token in _FALSE_VALUES:
            return False
    raise DisplayConfigError(
        f"{variable} must be a strict boolean "
        "(1/0, true/false, yes/no, or on/off)",
        variable=variable,
    )


def _parse_size(value: Any, variable: str) -> int:
    if isinstance(value, bool):
        raise DisplayConfigError(
            f"{variable} must be a non-negative integer byte count",
            variable=variable,
        )
    if isinstance(value, int):
        number = value
    elif isinstance(value, str):
        token = value.strip()
        if not token.isdigit():
            raise DisplayConfigError(
                f"{variable} must be a non-negative integer byte count",
                variable=variable,
            )
        number = int(token)
    else:
        raise DisplayConfigError(
            f"{variable} must be a non-negative integer byte count",
            variable=variable,
        )
    if number < 0:
        raise DisplayConfigError(
            f"{variable} must be a non-negative integer byte count",
            variable=variable,
        )
    return number


def _parse_sync_mode(value: Any, variable: str) -> str:
    if not isinstance(value, str):
        raise DisplayConfigError(
            f"{variable} must be one of {sorted(SYNC_MODES)}",
            variable=variable,
        )
    mode = value.strip().lower()
    if mode not in SYNC_MODES:
        raise DisplayConfigError(
            f"{variable} must be one of {sorted(SYNC_MODES)}; got {value!r}",
            variable=variable,
        )
    return mode


def _parse_profile(
    value: Any, variable: str = "OZMA_DISPLAY_PROFILE"
) -> str:
    if not isinstance(value, str):
        raise DisplayConfigError(
            f"{variable} must be one of {sorted(PROFILES)}",
            variable=variable,
        )
    profile = value.strip().lower()
    if profile not in PROFILES:
        raise DisplayConfigError(
            f"{variable} must be one of {sorted(PROFILES)}; got {value!r}",
            variable=variable,
        )
    return profile


def _parse_text(value: Any) -> str:
    if value is None:
        return ""
    return str(value).strip()


@dataclass(frozen=True)
class DisplayConfig:
    """Resolved display-platform rollout profile, feature flags, and limits.

    The default ``profile="local"`` enables the lossless IR, font translation,
    and the local asset repository while keeping importers and network sync off.
    Any feature flag passed explicitly overrides the profile.  The unsigned-pack
    and signature-policy fields are never changed by the profile.
    """

    ir_enabled: bool = _UNSET
    asset_repository_enabled: bool = _UNSET
    sync_mode: str = "offline"
    fonts_translation_enabled: bool = _UNSET
    importers_enabled: bool = _UNSET
    asset_index_url: str = ""
    cache_dir: str = ""
    allow_unsigned_packs: bool = False
    signature_required: bool = False
    #: Raw JSON string of trusted public keys.  Kept opaque here so runtimes can
    #: choose their own verifier format without this module parsing secrets.
    trusted_keys: str = ""
    max_pack_bytes: int = DEFAULT_MAX_PACK_BYTES
    max_import_bytes: int = DEFAULT_MAX_IMPORT_BYTES
    profile: str = DEFAULT_PROFILE

    def __post_init__(self) -> None:
        profile = _parse_profile(self.profile, "OZMA_DISPLAY_PROFILE")
        object.__setattr__(self, "profile", profile)
        defaults = _PROFILE_DEFAULTS[profile]
        for name in _PROFILE_FEATURE_FIELDS:
            value = getattr(self, name)
            if value is _UNSET:
                object.__setattr__(self, name, defaults[name])
        for name in _BOOL_FIELDS:
            if not isinstance(getattr(self, name), bool):
                raise DisplayConfigError(
                    f"{name} must be a boolean", variable=name
                )
        if self.sync_mode not in SYNC_MODES:
            raise DisplayConfigError(
                f"sync_mode must be one of {sorted(SYNC_MODES)}",
                variable="sync_mode",
            )
        for name in _SIZE_FIELDS:
            value = getattr(self, name)
            if isinstance(value, bool) or not isinstance(value, int):
                raise DisplayConfigError(
                    f"{name} must be a non-negative integer", variable=name
                )
            if value < 0:
                raise DisplayConfigError(
                    f"{name} must be a non-negative integer", variable=name
                )
        for name in _STRING_FIELDS:
            if not isinstance(getattr(self, name), str):
                raise DisplayConfigError(
                    f"{name} must be a string", variable=name
                )

    @classmethod
    def from_env(
        cls, environ: Mapping[str, str] | None = None
    ) -> "DisplayConfig":
        """Build a config from ``OZMA_DISPLAY_*`` variables.

        Unknown environment variables are ignored so callers can pass the
        process environment directly.  Invalid values raise
        :class:`DisplayConfigError` naming the offending variable.
        """
        source = os.environ if environ is None else environ
        return cls.from_mapping(source)

    @classmethod
    def from_mapping(
        cls, values: Mapping[str, Any] | None = None
    ) -> "DisplayConfig":
        """Build a config from field names or ``OZMA_DISPLAY_*`` keys.

        The profile supplies defaults for flags that are not set explicitly;
        explicit keys always win.  This is also how ``to_dict()`` round-trips.
        """
        resolved: dict[str, Any] = {}
        for raw_key, raw_value in dict(values or {}).items():
            key = str(raw_key)
            field = _ENV_TO_FIELD.get(key, key if key in _FIELD_NAMES else None)
            if field is not None:
                resolved[field] = raw_value

        profile = _parse_profile(
            resolved.get("profile", DEFAULT_PROFILE),
            "OZMA_DISPLAY_PROFILE",
        )
        profile_defaults = _PROFILE_DEFAULTS[profile]
        defaults = cls(profile=profile)

        def feature(name: str, variable: str) -> Any:
            if name in resolved:
                return _parse_bool(resolved[name], variable)
            return _UNSET

        return cls(
            ir_enabled=feature("ir_enabled", "OZMA_DISPLAY_IR_ENABLED"),
            asset_repository_enabled=feature(
                "asset_repository_enabled",
                "OZMA_DISPLAY_ASSET_REPOSITORY_ENABLED",
            ),
            sync_mode=_parse_sync_mode(
                resolved.get("sync_mode", profile_defaults["sync_mode"]),
                "OZMA_DISPLAY_SYNC_MODE",
            ),
            fonts_translation_enabled=feature(
                "fonts_translation_enabled",
                "OZMA_DISPLAY_FONTS_TRANSLATION_ENABLED",
            ),
            importers_enabled=feature(
                "importers_enabled", "OZMA_DISPLAY_IMPORTERS_ENABLED"
            ),
            asset_index_url=_parse_text(
                resolved.get("asset_index_url", defaults.asset_index_url)
            ),
            cache_dir=_parse_text(
                resolved.get("cache_dir", defaults.cache_dir)
            ),
            allow_unsigned_packs=_parse_bool(
                resolved.get(
                    "allow_unsigned_packs",
                    defaults.allow_unsigned_packs,
                ),
                "OZMA_DISPLAY_ALLOW_UNSIGNED_PACKS",
            ),
            signature_required=_parse_bool(
                resolved.get(
                    "signature_required",
                    defaults.signature_required,
                ),
                "OZMA_DISPLAY_SIGNATURE_REQUIRED",
            ),
            trusted_keys=_parse_text(
                resolved.get("trusted_keys", defaults.trusted_keys)
            ),
            max_pack_bytes=_parse_size(
                resolved.get("max_pack_bytes", defaults.max_pack_bytes),
                "OZMA_DISPLAY_MAX_PACK_BYTES",
            ),
            max_import_bytes=_parse_size(
                resolved.get("max_import_bytes", defaults.max_import_bytes),
                "OZMA_DISPLAY_MAX_IMPORT_BYTES",
            ),
            profile=profile,
        )

    def to_dict(self) -> dict[str, Any]:
        """Return a plain, JSON-serialisable mapping of every resolved field."""
        return asdict(self)

    def feature_flags(self) -> dict[str, Any]:
        """Map this config onto the ``display.*`` flag names from the plan."""
        return {
            "display.ir.enabled": self.ir_enabled,
            "display.asset_repository.enabled": self.asset_repository_enabled,
            "display.asset_sync.mode": self.sync_mode,
            "display.fonts.translation": (
                "enabled" if self.fonts_translation_enabled else "disabled"
            ),
            "display.importers.enabled": self.importers_enabled,
        }
