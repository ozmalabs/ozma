# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Importer registry and detection.

Modules are imported lazily so a partial deployment can still use the formats
it ships. Import failure is never silently ignored for a requested format.
"""

from __future__ import annotations

from pathlib import Path
from typing import Iterable

from .base import BaseImporter, ImportContext, ImportError, ImportResult

_CANDIDATE_CLASSES: tuple[tuple[str, str], ...] = (
    ("aida64", "Aida64Importer"),
    ("rainmeter", "RainmeterImporter"),
    ("home_assistant", "HomeAssistantImporter"),
    ("magicmirror", "MagicMirrorImporter"),
    ("grafana", "GrafanaImporter"),
    ("adaptive_cards", "AdaptiveCardsImporter"),
)


def available_importers() -> list[BaseImporter]:
    """Return every importer that can be imported in this deployment."""
    found: list[BaseImporter] = []
    for module_name, class_name in _CANDIDATE_CLASSES:
        try:
            module = __import__(
                f"{__package__}.{module_name}",
                fromlist=[class_name],
            )
        except ImportError:
            continue
        cls = getattr(module, class_name, None)
        if cls is None:
            continue
        found.append(cls())
    return found


def importer_for_format(fmt: str) -> BaseImporter:
    """Return the importer whose ``format`` or suffix matches ``fmt``."""
    token = str(fmt or "").strip().lower()
    if not token:
        raise ImportError("source format is required")
    for importer in available_importers():
        names = {importer.format.lower(), *(suffix.lower() for suffix in importer.suffixes)}
        normalised = {name.lstrip(".") for name in names}
        if token in names or token.lstrip(".") in normalised:
            return importer
        # Accept family aliases such as ``rainmeter`` for ``rainmeter_ini``.
        if len(token) >= 3 and (
            importer.format.lower().startswith(token)
            or token.startswith(importer.format.lower())
        ):
            return importer
    raise ImportError(f"no importer registered for `{fmt}`")


def detect_importer(name: str, data: bytes) -> BaseImporter:
    """Return the first importer that recognizes ``name``/``data``."""
    for importer in available_importers():
        try:
            if importer.detect(name, data):
                return importer
        except (ValueError, UnicodeError):
            continue
    raise ImportError(f"no importer recognized `{name}`")


def import_source(
    name: str,
    data: bytes,
    *,
    format: str | None = None,
    context: ImportContext | None = None,
) -> ImportResult:
    """Import bytes with an explicit format or content/type detection."""
    importer = importer_for_format(format) if format else detect_importer(name, data)
    result = importer.import_source(name, data, context)
    result.validate()
    return result


def import_path(path: str | Path, *, format: str | None = None) -> ImportResult:
    """Import a local file without executing it."""
    source = Path(path)
    return import_source(
        source.name,
        source.read_bytes(),
        format=format,
        context=ImportContext(name=source.name, base_dir=str(source.parent)),
    )


def importer_formats(importers: Iterable[BaseImporter] | None = None) -> list[dict[str, object]]:
    """Describe registered importers deterministically."""
    items = list(importers if importers is not None else available_importers())
    return [
        {
            "format": importer.format,
            "version": importer.version,
            "suffixes": list(importer.suffixes),
            "mime": importer.mime,
        }
        for importer in sorted(items, key=lambda item: item.format)
    ]


__all__ = [
    "available_importers",
    "detect_importer",
    "import_path",
    "import_source",
    "importer_for_format",
    "importer_formats",
]
