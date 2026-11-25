# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Runtime assembly for the display platform.

``DisplayConfig`` is the single source of truth for the ``OZMA_DISPLAY_*``
flags.  This module turns a resolved config into the objects the API needs:
the local asset repository, the private import store, and a small feature-gate
facade.  Construction is deliberately offline: it only reads local cache
metadata and never performs a sync, download, or signature-key lookup.

Nothing here holds module-level singletons.  Call :func:`build_runtime` once per
application (or per test) so different configurations can coexist.
"""

from __future__ import annotations

import json
from collections.abc import Mapping
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable

try:  # controller modules are imported top-level (controller/ is on sys.path)
    from asset_repository import (
        AssetRepository,
        AssetSyncPolicy,
        RepositoryLimits,
    )
    from asset_trust import TrustError, TrustStore, build_verifier
    from display_config import DisplayConfig, DisplayConfigError
    from display_import_store import ImportStore
except ImportError:  # package-style import used by some tooling
    from .asset_repository import (
        AssetRepository,
        AssetSyncPolicy,
        RepositoryLimits,
    )
    from .asset_trust import TrustError, TrustStore, build_verifier
    from .display_config import DisplayConfig, DisplayConfigError
    from .display_import_store import ImportStore

#: Map public feature names (and config field names) to ``DisplayConfig`` bools.
_FEATURE_FIELDS = {
    "ir": "ir_enabled",
    "ir_enabled": "ir_enabled",
    "display.ir.enabled": "ir_enabled",
    "asset_repository": "asset_repository_enabled",
    "asset_repository_enabled": "asset_repository_enabled",
    "display.asset_repository.enabled": "asset_repository_enabled",
    "fonts": "fonts_translation_enabled",
    "fonts_translation": "fonts_translation_enabled",
    "fonts_translation_enabled": "fonts_translation_enabled",
    "display.fonts.translation": "fonts_translation_enabled",
    "importers": "importers_enabled",
    "importers_enabled": "importers_enabled",
    "display.importers.enabled": "importers_enabled",
}


class DisplayDisabledError(RuntimeError):
    """Raised when a caller uses a display feature that is switched off."""

    status_code = 503
    code = "display_disabled"

    def __init__(self, feature: str) -> None:
        self.feature = feature
        super().__init__(f"display feature is disabled: {feature}")

    def to_detail(self) -> dict[str, str]:
        return {"code": self.code, "message": str(self)}


@dataclass
class DisplayRuntime:
    """Config plus the long-lived objects shared by display API routers."""

    config: DisplayConfig
    asset_repository: AssetRepository
    import_store: ImportStore

    def enabled(self, name: str) -> bool:
        """Return whether a display feature is enabled.

        Accepts the short names used by the API (``ir``, ``asset_repository``,
        ``fonts_translation``, ``importers``), their config field names, and the
        dotted plan names from ``docs/display-platform-execution-plan.md``.
        """
        field = _FEATURE_FIELDS.get(name)
        if field is None:
            raise ValueError(f"unknown display feature: {name!r}")
        return bool(getattr(self.config, field))

    def require(self, name: str) -> None:
        """Raise :class:`DisplayDisabledError` unless ``name`` is enabled."""
        if not self.enabled(name):
            raise DisplayDisabledError(name)

    def status(self) -> dict[str, Any]:
        """Return a sanitised, JSON-serialisable status document.

        The document deliberately reports *presence* for URLs, cache paths, and
        trusted-key configuration.  It never emits the index URL, filesystem
        paths, keys, tokens, or any other secret material.
        """
        counts: dict[str, int] = {
            "asset_index_packs": 0,
            "asset_index_versions": 0,
            "installed_packs": 0,
            "imports": 0,
        }
        try:
            index_stats = self.asset_repository.index.stats()
            counts["asset_index_packs"] = int(index_stats.get("packs", 0))
            counts["asset_index_versions"] = int(index_stats.get("versions", 0))
        except Exception:  # corrupt/absent cache must not break the status API
            pass
        try:
            counts["installed_packs"] = len(self.asset_repository.installed_ids())
        except Exception:
            pass
        try:
            counts["imports"] = len(self.import_store.list_imports())
        except Exception:
            pass

        return {
            "features": {
                "ir": self.enabled("ir"),
                "asset_repository": self.enabled("asset_repository"),
                "fonts_translation": self.enabled("fonts_translation"),
                "importers": self.enabled("importers"),
            },
            "feature_flags": self.config.feature_flags(),
            "sync_mode": self.config.sync_mode,
            "asset_index_url_configured": bool(
                self.config.asset_index_url.strip()
            ),
            "cache_dir_configured": bool(self.config.cache_dir.strip()),
            "trusted_keys_configured": bool(self.config.trusted_keys.strip()),
            "limits": {
                "max_pack_bytes": self.config.max_pack_bytes,
                "max_import_bytes": self.config.max_import_bytes,
            },
            "counts": counts,
        }


def _verifier_from_config(
    config: DisplayConfig,
) -> Callable[..., bool] | None:
    """Build a repository verifier from ``OZMA_DISPLAY_TRUSTED_KEYS``.

    The config keeps the trust store as opaque JSON. Invalid stores are a
    startup configuration error rather than a silent fallback to unsigned
    installs.
    """
    raw = str(config.trusted_keys or "").strip()
    if not raw:
        return None
    try:
        parsed = json.loads(raw)
        store = TrustStore.from_mapping(parsed)
        return build_verifier(store)
    except (ValueError, TypeError, TrustError) as exc:
        raise DisplayConfigError(
            f"invalid OZMA_DISPLAY_TRUSTED_KEYS: {exc}",
            variable="OZMA_DISPLAY_TRUSTED_KEYS",
        ) from exc


def build_runtime(
    config: DisplayConfig | Mapping[str, Any],
    *,
    packs_dir: str | Path | None = None,
    cache_dir: str | Path | None = None,
    transport: Callable[..., Any] | None = None,
    signature_verifier: Callable[..., bool] | None = None,
) -> DisplayRuntime:
    """Assemble a :class:`DisplayRuntime` without touching the network.

    ``cache_dir`` (when supplied) overrides ``config.cache_dir``; the import
    store lives under ``<cache_dir>/imports``.  ``transport`` and
    ``signature_verifier`` are injected so tests and deployments can avoid
    network and key-distribution side effects.
    """
    if isinstance(config, Mapping):
        config = DisplayConfig.from_mapping(config)
    if not isinstance(config, DisplayConfig):
        raise DisplayConfigError("display config must be a DisplayConfig")

    if config.max_import_bytes <= 0:
        raise DisplayConfigError(
            "max_import_bytes must be positive",
            variable="OZMA_DISPLAY_MAX_IMPORT_BYTES",
        )

    effective_cache = cache_dir or config.cache_dir or None
    cache_root = Path(effective_cache) if effective_cache is not None else None
    import_root = cache_root / "imports" if cache_root is not None else None

    limits = RepositoryLimits(
        max_file_bytes=config.max_pack_bytes,
        max_total_bytes=config.max_pack_bytes,
    )
    policy = AssetSyncPolicy(
        mode=config.sync_mode,
        verify_signatures=config.signature_required,
    )

    if signature_verifier is None:
        signature_verifier = _verifier_from_config(config)

    asset_repository = AssetRepository(
        packs_dir=packs_dir,
        cache_dir=cache_root,
        policy=policy,
        signature_verifier=signature_verifier,
        allow_unsigned=config.allow_unsigned_packs,
        require_signature=config.signature_required,
        limits=limits,
        transport=transport,
        index_url=config.asset_index_url or None,
    )
    import_store = ImportStore(
        import_root,
        max_import_bytes=config.max_import_bytes,
    )

    return DisplayRuntime(
        config=config,
        asset_repository=asset_repository,
        import_store=import_store,
    )


def build_config_router(runtime: DisplayRuntime):
    """Build the read-only ``/api/v1/display/config`` router."""
    from fastapi import APIRouter

    router = APIRouter(prefix="/api/v1/display", tags=["display"])

    @router.get("/config")
    def get_display_config() -> dict[str, Any]:
        return runtime.status()

    return router


__all__ = [
    "DisplayDisabledError",
    "DisplayRuntime",
    "build_config_router",
    "build_runtime",
]
