# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Shared contract for lossless display importers.

Importers never execute source code. Scripts, unknown fields, and unsupported
structures are retained opaquely in the source record. The semantic widget
projection is intentionally best-effort; the raw source remains authoritative.
"""

from __future__ import annotations

import hashlib
from abc import ABC, abstractmethod
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Mapping

IMPORT_SCHEMA = "org.ozma.import/1"

#: Widget kinds accepted by the IR contract.
WIDGET_KINDS = frozenset(
    {
        "gauge",
        "bar",
        "number",
        "text",
        "sparkline",
        "chart",
        "status",
        "icon",
        "table",
        "card",
        "custom",
    }
)


class ImportError(ValueError):
    """Raised when a source cannot be parsed at all.

    Unsupported constructs inside an otherwise readable source are reported in
    ``warnings``/``extensions`` instead of raising.
    """


def sha256_hex(data: bytes | bytearray | memoryview) -> str:
    """Return the lowercase SHA-256 hex digest of ``data``."""
    return hashlib.sha256(bytes(data)).hexdigest()


def _mime_for_name(name: str, fallback: str = "application/octet-stream") -> str:
    suffixes = {
        ".json": "application/json",
        ".yaml": "application/yaml",
        ".yml": "application/yaml",
        ".ini": "text/plain",
        ".txt": "text/plain",
        ".xml": "application/xml",
        ".html": "text/html",
        ".css": "text/css",
        ".js": "text/javascript",
        ".png": "image/png",
        ".jpg": "image/jpeg",
        ".jpeg": "image/jpeg",
        ".svg": "image/svg+xml",
        ".ttf": "font/ttf",
        ".otf": "font/otf",
        ".woff": "font/woff",
        ".woff2": "font/woff2",
        ".zip": "application/zip",
    }
    return suffixes.get(Path(name).suffix.lower(), fallback)


@dataclass
class ImportAsset:
    """A source asset retained byte-for-byte."""

    path: str
    data: bytes
    mime: str = ""
    kind: str = "original"
    executable: bool = False
    sandbox: str = ""
    id: str = ""

    def __post_init__(self) -> None:
        self.path = str(self.path)
        if not self.mime:
            self.mime = _mime_for_name(self.path)
        if not self.id:
            self.id = f"asset-{sha256_hex(self.data)[:16]}"

    @property
    def size(self) -> int:
        return len(self.data)

    @property
    def sha256(self) -> str:
        return sha256_hex(self.data)

    def to_asset_ref(self) -> dict[str, Any]:
        """Return a contract ``AssetRef`` (no bytes)."""
        return {
            "id": self.id,
            "path": self.path,
            "mime": self.mime,
            "sha256": self.sha256,
            "size": self.size,
            "kind": self.kind,
            "executable": bool(self.executable),
            "sandbox": self.sandbox,
        }

    def to_source_asset(self) -> dict[str, Any]:
        """Return a ``SourceDocument.original`` asset record."""
        return self.to_asset_ref()


def make_source_document(
    source_format: str,
    source_version: str,
    raw: bytes,
    *,
    original: ImportAsset | Mapping[str, Any] | None = None,
    parsed: Any = None,
    unknown: Any = None,
) -> dict[str, Any]:
    """Build a lossless :class:`SourceDocument` mapping."""
    if not source_format:
        raise ImportError("source format is required")
    if isinstance(original, ImportAsset):
        original_ref: dict[str, Any] | None = original.to_source_asset()
    else:
        original_ref = dict(original) if original else None
    return {
        "format": str(source_format),
        "version": str(source_version or ""),
        "source_hash": sha256_hex(raw),
        "original": original_ref,
        "parsed": parsed,
        "unknown": unknown if unknown is not None else {},
    }


def _captured_features(captured: Any) -> list[str]:
    if captured is None or captured is False:
        return []
    if captured is True:
        return ["source"]
    if isinstance(captured, str):
        return [captured]
    if isinstance(captured, (list, tuple, set, frozenset)):
        return [str(item) for item in captured]
    return ["source"]


def _opaque_names(opaque: Any) -> list[str]:
    if opaque is None:
        return []
    if isinstance(opaque, str):
        return [opaque]
    if isinstance(opaque, Mapping):
        return [str(key) for key in opaque]
    if isinstance(opaque, (list, tuple, set, frozenset)):
        return [str(item) for item in opaque]
    return []


def make_source_capture(
    source_format: str,
    source_version: str,
    raw: bytes,
    *,
    assets: list[ImportAsset] | None = None,
    captured: Any = None,
    opaque: Any = None,
    warnings: list[str] | None = None,
) -> dict[str, Any]:
    """Build a :class:`SourceCaptureReport`; imports are always lossless.

    ``captured``, ``opaque``, and ``assets`` use the canonical string-array
    shape from ``schemas/display/source-v1.schema.json``. The full opaque data
    and asset references are retained in additional fields so nothing is lost.
    """
    asset_list = list(assets or [])
    return {
        "source_format": str(source_format),
        "source_version": str(source_version or ""),
        "source_hash": sha256_hex(raw),
        "captured": _captured_features(captured),
        "opaque": _opaque_names(opaque),
        "assets": [asset.id for asset in asset_list],
        "lossless": True,
        "asset_refs": [asset.to_asset_ref() for asset in asset_list],
        "opaque_data": opaque if opaque is not None else {},
        "warnings": list(warnings or []),
    }


def make_widget(
    widget_id: str,
    kind: str,
    source_format: str,
    raw: bytes,
    *,
    metric: str = "",
    role: str = "",
    label: str = "",
    domain: Mapping[str, Any] | None = None,
    layout: Mapping[str, Any] | None = None,
    style: Mapping[str, Any] | None = None,
    typography: Mapping[str, Any] | None = None,
    vibe: Mapping[str, Any] | None = None,
    shape: Mapping[str, Any] | None = None,
    accessibility: Mapping[str, Any] | None = None,
    extensions: Mapping[str, Any] | None = None,
    source_version: str = "",
    source_unknown: Any = None,
) -> dict[str, Any]:
    """Build a minimal valid :class:`WidgetIR` mapping.

    ``metric`` defaults to the widget id so that a source with no explicit data
    binding can still round-trip through the IR contract.
    """
    if kind not in WIDGET_KINDS:
        raise ImportError(f"unknown widget kind `{kind}`")
    widget: dict[str, Any] = {
        "id": str(widget_id),
        "kind": kind,
        "binding": {"metric": str(metric or widget_id)},
        "source": make_source_document(
            source_format,
            source_version,
            raw,
            unknown=source_unknown,
        ),
        "extensions": dict(extensions or {}),
    }
    if role:
        widget["role"] = str(role)
    if label:
        widget["label"] = str(label)
    if domain:
        widget["domain"] = dict(domain)
    if layout:
        widget["layout"] = dict(layout)
    if style:
        widget["style"] = dict(style)
    if typography:
        widget["typography"] = dict(typography)
    if vibe:
        widget["vibe"] = dict(vibe)
    if shape:
        widget["shape"] = dict(shape)
    if accessibility:
        widget["accessibility"] = dict(accessibility)
    return widget


@dataclass
class ImportContext:
    """Optional context for an import run."""

    name: str = ""
    mime: str = ""
    base_dir: str = ""
    private: bool = True
    options: dict[str, Any] = field(default_factory=dict)


@dataclass
class ImportResult:
    """The complete, lossless result of importing one source."""

    format: str
    version: str
    raw: bytes
    widgets: list[dict[str, Any]] = field(default_factory=list)
    source_document: dict[str, Any] = field(default_factory=dict)
    source_capture: dict[str, Any] = field(default_factory=dict)
    assets: list[ImportAsset] = field(default_factory=list)
    unknown: dict[str, Any] = field(default_factory=dict)
    warnings: list[str] = field(default_factory=list)
    extensions: dict[str, Any] = field(default_factory=dict)

    @property
    def lossless(self) -> bool:
        return bool(self.source_capture.get("lossless"))

    def asset_refs(self) -> list[dict[str, Any]]:
        return [asset.to_asset_ref() for asset in self.assets]

    def to_dict(self, *, include_bytes: bool = False) -> dict[str, Any]:
        data: dict[str, Any] = {
            "schema": IMPORT_SCHEMA,
            "format": self.format,
            "version": self.version,
            "lossless": self.lossless,
            "widgets": [dict(widget) for widget in self.widgets],
            "source": dict(self.source_document),
            "source_capture": dict(self.source_capture),
            "assets": self.asset_refs(),
            "unknown": dict(self.unknown),
            "warnings": list(self.warnings),
            "extensions": dict(self.extensions),
        }
        if include_bytes:
            data["raw"] = bytes(self.raw)
            data["asset_bytes"] = [
                {"path": asset.path, "sha256": asset.sha256, "data": bytes(asset.data)}
                for asset in self.assets
            ]
        return data

    def validate(self) -> None:
        """Validate the lossless invariants and every produced widget."""
        if not self.lossless:
            raise ImportError("import capture must be lossless")
        # Local import keeps this module usable without controller package setup.
        import display_ir  # type: ignore[import-not-found]

        display_ir.validate_source_capture(self.source_capture)
        display_ir.validate_source(self.source_document)
        for widget in self.widgets:
            display_ir.validate_ir(widget)


class BaseImporter(ABC):
    """Base class for source importers.

    Subclasses implement :meth:`parse`. ``format`` is the source format name
    stored in the IR source record (for example ``aida64.rslcd``).
    """

    format: str = ""
    version: str = ""
    suffixes: tuple[str, ...] = ()
    mime: str = "application/octet-stream"

    def detect(self, name: str, data: bytes) -> bool:
        """Return whether this importer recognizes the source."""
        suffix = Path(name).suffix.lower()
        if self.suffixes and suffix in self.suffixes:
            return True
        return self.sniff(data)

    def sniff(self, data: bytes) -> bool:  # pragma: no cover - default
        """Content-based detection hook; defaults to false."""
        return False

    def import_path(self, path: str | Path) -> ImportResult:
        source = Path(path)
        data = source.read_bytes()
        return self.import_source(source.name, data, ImportContext(name=source.name))

    def import_source(
        self,
        name: str,
        data: bytes,
        context: ImportContext | None = None,
    ) -> ImportResult:
        if not isinstance(data, (bytes, bytearray, memoryview)):
            raise ImportError("source data must be bytes")
        raw = bytes(data)
        result = self.parse(name, raw, context or ImportContext(name=name))
        if not result.source_capture:
            result.source_capture = make_source_capture(
                result.format or self.format,
                result.version or self.version,
                raw,
                assets=result.assets,
                opaque=result.unknown,
                warnings=result.warnings,
            )
        if not result.source_document:
            result.source_document = make_source_document(
                result.format or self.format,
                result.version or self.version,
                raw,
                unknown=result.unknown,
            )
        if not result.lossless:
            raise ImportError("importer produced a lossy capture")
        return result

    @abstractmethod
    def parse(
        self,
        name: str,
        data: bytes,
        context: ImportContext,
    ) -> ImportResult:
        """Parse ``data`` into a lossless :class:`ImportResult`."""
