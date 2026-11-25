# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Lossless display importers.

Every importer returns an :class:`ImportResult`: a source document, a capture
report with ``lossless: true``, retained raw assets, and a best-effort list of
:class:`WidgetIR` mappings. Unknown source fields and opaque payloads are kept
in ``extensions``; nothing is discarded on import.
"""

from __future__ import annotations

from .base import (
    IMPORT_SCHEMA,
    BaseImporter,
    ImportAsset,
    ImportContext,
    ImportError,
    ImportResult,
    make_source_capture,
    make_source_document,
    make_widget,
    sha256_hex,
)

__all__ = [
    "IMPORT_SCHEMA",
    "BaseImporter",
    "ImportAsset",
    "ImportContext",
    "ImportError",
    "ImportResult",
    "make_source_capture",
    "make_source_document",
    "make_widget",
    "sha256_hex",
]
