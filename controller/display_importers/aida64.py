# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Lossless AIDA64 ``.rslcd`` / ``.sensorpanel`` importer.

The native AIDA64 panel formats are only partially documented, so this
importer treats the source document as authoritative. It recognises ZIP
containers plus XML, JSON, and HTML payloads, projects the sensor/gauge/bar
elements it understands into :class:`WidgetIR`, and keeps everything else as
opaque extensions and assets. Embedded scripts and styles are retained but are
never executed.
"""

from __future__ import annotations

import base64
import io
import json
import math
import re
import zipfile
from pathlib import Path
from typing import Any, Mapping
from xml.etree import ElementTree

from .base import (
    BaseImporter,
    ImportAsset,
    ImportContext,
    ImportError,
    ImportResult,
    make_source_capture,
    make_source_document,
    make_widget,
)

# Archive limits. These are deliberately conservative: imports are bounded and
# a hostile archive is rejected instead of being partially expanded.
MAX_MEMBERS = 4096
MAX_MEMBER_SIZE = 64 * 1024 * 1024
MAX_TOTAL_UNCOMPRESSED = 256 * 1024 * 1024
MAX_COMPRESSION_RATIO = 1000

_SCRIPT_SUFFIXES = {
    ".js",
    ".mjs",
    ".lua",
    ".vbs",
    ".vbe",
    ".py",
    ".ps1",
    ".bat",
    ".cmd",
    ".sh",
}
_IMAGE_SUFFIXES = {".png", ".jpg", ".jpeg", ".gif", ".bmp", ".webp", ".svg", ".ico"}
_FONT_SUFFIXES = {".ttf", ".otf", ".woff", ".woff2", ".ttc"}
_PAYLOAD_SUFFIXES = {
    ".xml",
    ".json",
    ".rslcd",
    ".sensorpanel",
    ".html",
    ".htm",
    ".txt",
}

_XML_KINDS = {
    "gauge": "gauge",
    "bar": "bar",
    "progressbar": "bar",
    "progress": "bar",
    "label": "text",
    "text": "text",
    "statictext": "text",
    "sensor": "number",
    "sensorvalue": "number",
    "value": "number",
    "number": "number",
    "meter": "gauge",
}

_COLOR_KEYS = {
    "color",
    "colour",
    "fgcolor",
    "foreground",
    "barcolor",
    "fillcolor",
    "textcolor",
    "background",
    "bgcolor",
}

_SIMPLE_ID_RE = re.compile(
    r"<(?:span|div)[^>]*(?:id|class)\s*=\s*[^>]*?\b(Simple\d+)\b[^>]*>"
    r"(?P<body>.*?)</(?:span|div)>",
    re.IGNORECASE | re.DOTALL,
)
_TAG_RE = re.compile(r"<[^>]+>")
_SCRIPT_RE = re.compile(r"(?is)<script\b(?P<attrs>[^>]*)>(?P<body>.*?)</script>")
_STYLE_RE = re.compile(r"(?is)<style\b[^>]*>(?P<body>.*?)</style>")


def _local_name(name: str) -> str:
    return name.rsplit("}", 1)[-1]


def _attr(attributes: Mapping[str, Any], *names: str) -> Any:
    lowered = {str(key).lower(): value for key, value in attributes.items()}
    localised = {_local_name(str(key)).lower(): value for key, value in attributes.items()}
    for name in names:
        if name in lowered:
            return lowered[name]
        if name in localised:
            return localised[name]
    return None


def _number(value: Any) -> float | None:
    if value is None or isinstance(value, bool):
        return None
    if isinstance(value, (int, float)):
        number = float(value)
    else:
        text = str(value).strip()
        if not text:
            return None
        try:
            number = float(text)
        except ValueError:
            return None
    return number if math.isfinite(number) else None


def _element_to_dict(element: ElementTree.Element) -> dict[str, Any]:
    return {
        "tag": element.tag,
        "attributes": dict(element.attrib),
        "text": element.text,
        "children": [_element_to_dict(child) for child in list(element)],
    }


def _slug(value: str) -> str:
    slug = re.sub(r"[^A-Za-z0-9_.-]+", "-", value).strip("-")
    return slug or "item"


class Aida64Importer(BaseImporter):
    """Import AIDA64 ``.rslcd`` and ``.sensorpanel`` documents."""

    format = "aida64_rslcd"
    version = "1"
    suffixes = (".rslcd", ".sensorpanel")
    mime = "application/octet-stream"

    def sniff(self, data: bytes) -> bool:
        if not data:
            return False
        if data.startswith((b"PK\x03\x04", b"PK\x05\x06", b"PK\x07\x08")):
            return True
        if self._is_zip(data):
            return True
        head = data.lstrip()[:4096].lower()
        return head.startswith((b"<", b"{", b"["))

    def parse(
        self,
        name: str,
        data: bytes,
        context: ImportContext,
    ) -> ImportResult:
        source_format = self._source_format(name)
        raw = bytes(data)
        source_path = name or f"source{self.suffixes[0]}"
        original = ImportAsset(
            path=source_path, data=raw, kind="original", executable=False, sandbox="none"
        )
        assets: list[ImportAsset] = [original]
        warnings: list[str] = []
        unknown: dict[str, Any] = {
            "members": [],
            "payloads": [],
            "unmapped": [],
            "scripts": [],
            "styles": [],
            "opaque": [],
        }
        parsed: dict[str, Any] = {
            "format": source_format,
            "classification": "",
            "payloads": [],
        }
        widgets: list[dict[str, Any]] = []
        seen_widget_ids: set[str] = set()

        if self._is_zip(raw):
            members = self._read_zip(raw)
            parsed["classification"] = "zip"
            for member in members:
                assets.append(member)
                unknown["members"].append(member.to_asset_ref())
            unknown["opaque"].append("zip:members")
            payload_members = [
                member
                for member in members
                if Path(member.path).suffix.lower() in _PAYLOAD_SUFFIXES
            ]
            if not payload_members:
                warnings.append("archive contains no recognisable panel payload")
            for index, member in enumerate(payload_members):
                text = self._decode(member.data)
                before = len(unknown["payloads"])
                payload_widgets = self._parse_payload(
                    text,
                    raw=raw,
                    source_format=source_format,
                    source_name=member.path,
                    index_prefix=f"m{index}-",
                    assets=assets,
                    unknown=unknown,
                    warnings=warnings,
                    seen_widget_ids=seen_widget_ids,
                )
                parsed["payloads"].append(
                    self._payload_record(
                        member.path,
                        member.sha256,
                        self._classify(text),
                        payload_widgets,
                        unknown["payloads"][before:],
                    )
                )
                widgets.extend(payload_widgets)
        else:
            text = self._decode(raw)
            classification = self._classify(text)
            parsed["classification"] = classification
            before = len(unknown["payloads"])
            widgets = self._parse_payload(
                text,
                raw=raw,
                source_format=source_format,
                source_name=source_path,
                index_prefix="",
                assets=assets,
                unknown=unknown,
                warnings=warnings,
                seen_widget_ids=seen_widget_ids,
            )
            parsed["payloads"].append(
                self._payload_record(
                    source_path,
                    original.sha256,
                    classification,
                    widgets,
                    unknown["payloads"][before:],
                )
            )

        if not widgets:
            warnings.append(
                "no sensor/gauge/bar elements were mapped; raw payload retained opaque"
            )
            if not unknown["opaque"]:
                unknown["opaque"].append("payload:opaque")

        source_document = make_source_document(
            source_format,
            self.version,
            raw,
            original=original,
            parsed=parsed,
            unknown=unknown,
        )
        source_capture = make_source_capture(
            source_format,
            self.version,
            raw,
            assets=assets,
            opaque=list(dict.fromkeys(unknown["opaque"])),
            warnings=warnings,
        )
        extensions = {
            "aida64": {
                "classification": parsed["classification"],
                "member_count": len(unknown["members"]),
                "script_count": len(unknown["scripts"]),
                "style_count": len(unknown["styles"]),
                "members": unknown["members"],
                "payloads": parsed["payloads"],
                "unmapped": unknown["unmapped"],
                "scripts": unknown["scripts"],
                "styles": unknown["styles"],
                "opaque": unknown["opaque"],
            }
        }
        return ImportResult(
            format=source_format,
            version=self.version,
            raw=raw,
            widgets=widgets,
            source_document=source_document,
            source_capture=source_capture,
            assets=assets,
            unknown=unknown,
            warnings=warnings,
            extensions=extensions,
        )

    # ------------------------------------------------------------------
    # Container handling
    # ------------------------------------------------------------------
    def _source_format(self, name: str) -> str:
        suffix = Path(name).suffix.lower()
        if suffix == ".sensorpanel":
            return "aida64_sensorpanel"
        return "aida64_rslcd"

    @staticmethod
    def _payload_record(
        path: str,
        sha256: str,
        classification: str,
        widgets: list[dict[str, Any]],
        extracted: list[Any],
    ) -> dict[str, Any]:
        record: dict[str, Any] = {
            "path": path,
            "sha256": sha256,
            "kind": classification,
            "widget_count": len(widgets),
        }
        for entry in extracted:
            if "document" in entry:
                record["document"] = entry["document"]
            if "text" in entry and "document" not in record:
                record["text"] = entry["text"]
        return record

    @staticmethod
    def _decode(data: bytes) -> str:
        if data.startswith((b"\xff\xfe", b"\xfe\xff")):
            return data.decode("utf-16", errors="replace")
        return data.decode("utf-8-sig", errors="replace")

    @staticmethod
    def _classify(text: str) -> str:
        stripped = text.lstrip("\ufeff \t\r\n")
        if not stripped:
            return "empty"
        lower = stripped[:4096].lower()
        if lower.startswith("<!doctype html") or lower.startswith("<html") or "<html" in lower:
            return "html"
        if stripped.startswith("<"):
            return "xml"
        if stripped.startswith(("{", "[")):
            return "json"
        return "opaque"

    @staticmethod
    def _is_zip(data: bytes) -> bool:
        if not data.startswith(b"PK"):
            return False
        try:
            return zipfile.is_zipfile(io.BytesIO(data))
        except (OSError, ValueError):
            return False

    def _read_zip(self, raw: bytes) -> list[ImportAsset]:
        try:
            archive = zipfile.ZipFile(io.BytesIO(raw))
        except zipfile.BadZipFile as exc:
            raise ImportError(f"invalid AIDA64 archive: {exc}") from exc
        infos = archive.infolist()
        if len(infos) > MAX_MEMBERS:
            raise ImportError(
                f"archive has {len(infos)} members; limit is {MAX_MEMBERS}"
            )
        members: list[ImportAsset] = []
        total_uncompressed = 0
        actual_uncompressed = 0
        for info in infos:
            self._validate_member_name(info.filename)
            if info.is_dir():
                continue
            if info.flag_bits & 0x1:
                raise ImportError(f"encrypted archive member rejected: {info.filename}")
            file_type = (info.external_attr >> 16) & 0o170000
            if file_type == 0o120000:
                raise ImportError(f"symlink archive member rejected: {info.filename}")
            if info.file_size > MAX_MEMBER_SIZE:
                raise ImportError(
                    f"archive member too large: {info.filename} ({info.file_size} bytes)"
                )
            total_uncompressed += info.file_size
            if total_uncompressed > MAX_TOTAL_UNCOMPRESSED:
                raise ImportError(
                    "archive expands beyond the total uncompressed size limit"
                )
            with archive.open(info) as handle:
                content = handle.read(MAX_MEMBER_SIZE + 1)
            if len(content) > MAX_MEMBER_SIZE:
                raise ImportError(f"archive member too large: {info.filename}")
            actual_uncompressed += len(content)
            if actual_uncompressed > MAX_TOTAL_UNCOMPRESSED:
                raise ImportError(
                    "archive expands beyond the total uncompressed size limit"
                )
            if (
                info.compress_size > 0
                and len(content) > info.compress_size * MAX_COMPRESSION_RATIO
            ):
                raise ImportError(
                    f"archive member compression ratio rejected: {info.filename}"
                )
            members.append(
                ImportAsset(
                    path=info.filename,
                    data=content,
                    mime="",
                    kind=self._asset_kind(info.filename),
                    executable=False,
                    sandbox="none",
                )
            )
        return members

    @staticmethod
    def _validate_member_name(name: str) -> None:
        if not name or "\x00" in name:
            raise ImportError("archive member has an invalid name")
        normalised = name.replace("\\", "/")
        if normalised.startswith("/") or re.match(r"^[A-Za-z]:", normalised):
            raise ImportError(f"absolute archive member path rejected: {name}")
        parts = [part for part in normalised.split("/") if part not in ("", ".")]
        if any(part == ".." for part in parts):
            raise ImportError(f"archive member path traversal rejected: {name}")
        if not parts:
            raise ImportError(f"archive member has an empty path: {name}")

    @staticmethod
    def _asset_kind(name: str) -> str:
        suffix = Path(name).suffix.lower()
        if suffix in _SCRIPT_SUFFIXES:
            return "script"
        if suffix in _IMAGE_SUFFIXES:
            return "image"
        if suffix in _FONT_SUFFIXES:
            return "font"
        return "other"

    # ------------------------------------------------------------------
    # Payload handling
    # ------------------------------------------------------------------
    def _parse_payload(
        self,
        text: str,
        *,
        raw: bytes,
        source_format: str,
        source_name: str,
        index_prefix: str,
        assets: list[ImportAsset],
        unknown: dict[str, Any],
        warnings: list[str],
        seen_widget_ids: set[str],
    ) -> list[dict[str, Any]]:
        classification = self._classify(text)
        if classification == "xml":
            return self._parse_xml(
                text,
                raw=raw,
                source_format=source_format,
                source_name=source_name,
                index_prefix=index_prefix,
                assets=assets,
                unknown=unknown,
                warnings=warnings,
                seen_widget_ids=seen_widget_ids,
            )
        if classification == "json":
            return self._parse_json(
                text,
                raw=raw,
                source_format=source_format,
                source_name=source_name,
                index_prefix=index_prefix,
                unknown=unknown,
                warnings=warnings,
                seen_widget_ids=seen_widget_ids,
            )
        if classification == "html":
            return self._parse_html(
                text,
                raw=raw,
                source_format=source_format,
                source_name=source_name,
                index_prefix=index_prefix,
                assets=assets,
                unknown=unknown,
                warnings=warnings,
                seen_widget_ids=seen_widget_ids,
            )
        unknown["payloads"].append(
            {
                "path": source_name,
                "classification": classification,
                "text": text,
            }
        )
        unknown["opaque"].append(f"payload:{source_name}")
        return []

    def _parse_xml(
        self,
        text: str,
        *,
        raw: bytes,
        source_format: str,
        source_name: str,
        index_prefix: str,
        assets: list[ImportAsset],
        unknown: dict[str, Any],
        warnings: list[str],
        seen_widget_ids: set[str],
    ) -> list[dict[str, Any]]:
        try:
            root = ElementTree.fromstring(text)
        except ElementTree.ParseError as exc:
            warnings.append(f"XML payload could not be parsed: {exc}")
            unknown["payloads"].append(
                {"path": source_name, "classification": "xml", "error": str(exc), "text": text}
            )
            unknown["opaque"].append(f"payload:{source_name}")
            return []
        unknown["payloads"].append(
            {
                "path": source_name,
                "classification": "xml",
                "document": _element_to_dict(root),
            }
        )
        self._retain_embedded_scripts(
            text, source_name=source_name, assets=assets, unknown=unknown
        )
        widgets: list[dict[str, Any]] = []
        for index, element in enumerate(root.iter()):
            tag = _local_name(element.tag).lower()
            kind = self._xml_kind(tag)
            if kind is None:
                if tag not in {"sensorpanel", "panel", "root", "widgets"} and list(element):
                    unknown["unmapped"].append(_element_to_dict(element))
                continue
            if tag == "script":
                continue
            widget = self._element_widget(
                element,
                raw=raw,
                tag=tag,
                kind=kind,
                source_format=source_format,
                source_name=source_name,
                index_prefix=index_prefix,
                index=index,
                seen_widget_ids=seen_widget_ids,
            )
            if widget is not None:
                widgets.append(widget)
        return widgets

    @staticmethod
    def _xml_kind(tag: str) -> str | None:
        if tag in _XML_KINDS:
            return _XML_KINDS[tag]
        if tag.endswith("script") or tag.endswith("style"):
            return None
        for key, kind in _XML_KINDS.items():
            if not tag.startswith(key):
                continue
            suffix = tag[len(key):]
            # Numbered elements such as ``gauge1`` are recognised, but
            # container names such as ``sensorpanel`` are not.
            if suffix.isdigit() or (suffix.startswith("_") and suffix[1:].isdigit()):
                return kind
        return None

    def _parse_json(
        self,
        text: str,
        *,
        raw: bytes,
        source_format: str,
        source_name: str,
        index_prefix: str,
        unknown: dict[str, Any],
        warnings: list[str],
        seen_widget_ids: set[str],
    ) -> list[dict[str, Any]]:
        try:
            document = json.loads(text)
        except json.JSONDecodeError as exc:
            warnings.append(f"JSON payload could not be parsed: {exc}")
            unknown["payloads"].append(
                {"path": source_name, "classification": "json", "error": str(exc), "text": text}
            )
            unknown["opaque"].append(f"payload:{source_name}")
            return []
        unknown["payloads"].append(
            {
                "path": source_name,
                "classification": "json",
                "document": document,
            }
        )
        items: list[Any]
        if isinstance(document, list):
            items = document
        elif isinstance(document, Mapping):
            found = False
            for key in ("widgets", "elements", "items", "sensors", "gauges", "bars", "controls"):
                value = document.get(key)
                if isinstance(value, list):
                    items = value
                    found = True
                    break
                if isinstance(value, Mapping):
                    items = []
                    for name, entry in value.items():
                        if isinstance(entry, Mapping):
                            item = dict(entry)
                            item.setdefault("id", name)
                            items.append(item)
                        else:
                            items.append({"id": name, "value": entry})
                    found = True
                    break
            if not found:
                mapping_values = list(document.values())
                if mapping_values and all(
                    isinstance(value, Mapping) for value in mapping_values
                ):
                    items = []
                    for name, entry in document.items():
                        item = dict(entry)
                        item.setdefault("id", name)
                        items.append(item)
                else:
                    items = [document]
        else:
            warnings.append("JSON payload is not an object or array")
            return []
        widgets: list[dict[str, Any]] = []
        for index, item in enumerate(items):
            if not isinstance(item, Mapping):
                unknown["unmapped"].append(item)
                continue
            tag = str(
                item.get("type")
                or item.get("kind")
                or item.get("widget")
                or item.get("element")
                or item.get("control")
                or ""
            ).lower()
            kind = self._xml_kind(tag) or self._infer_json_kind(item)
            if kind is None:
                unknown["unmapped"].append(dict(item))
                continue
            widget = self._mapping_widget(
                item,
                raw=raw,
                tag=tag or kind,
                kind=kind,
                source_format=source_format,
                source_name=source_name,
                index_prefix=index_prefix,
                index=index,
                seen_widget_ids=seen_widget_ids,
            )
            if widget is not None:
                widgets.append(widget)
        return widgets

    @staticmethod
    def _infer_json_kind(item: Mapping[str, Any]) -> str | None:
        lowered_keys = {str(key).lower() for key in item}
        identifier = str(item.get("id") or item.get("name") or "").lower()
        if "gauge" in identifier:
            return "gauge"
        if "bar" in identifier:
            return "bar"
        if "label" in identifier:
            return "text"
        if "min" in lowered_keys and "max" in lowered_keys:
            return "gauge"
        if "value" in lowered_keys and "unit" in lowered_keys:
            return "number"
        return None

    def _parse_html(
        self,
        text: str,
        *,
        raw: bytes,
        source_format: str,
        source_name: str,
        index_prefix: str,
        assets: list[ImportAsset],
        unknown: dict[str, Any],
        warnings: list[str],
        seen_widget_ids: set[str],
    ) -> list[dict[str, Any]]:
        unknown["payloads"].append(
            {"path": source_name, "classification": "html", "text": text}
        )
        self._retain_embedded_scripts(
            text, source_name=source_name, assets=assets, unknown=unknown
        )
        widgets: list[dict[str, Any]] = []
        for index, match in enumerate(_SIMPLE_ID_RE.finditer(text)):
            simple_id = match.group(1)
            body = match.group("body") or ""
            label = _TAG_RE.sub("", body).strip()
            widget_id = self._unique_id(
                f"{index_prefix}aida64-{_slug(simple_id)}", seen_widget_ids
            )
            widgets.append(
                make_widget(
                    widget_id,
                    "number",
                    source_format,
                    raw,
                    metric=simple_id,
                    label=label or simple_id,
                    extensions={
                        "aida64": {
                            "kind": "html-simple-sensor",
                            "element": match.group(0),
                            "source": source_name,
                        }
                    },
                )
            )
        if not widgets:
            warnings.append("HTML payload contained no SimpleN sensor spans")
        return widgets

    def _retain_embedded_scripts(
        self,
        text: str,
        *,
        source_name: str,
        assets: list[ImportAsset],
        unknown: dict[str, Any],
    ) -> None:
        for index, match in enumerate(_SCRIPT_RE.finditer(text)):
            body = match.group("body") or ""
            if not body.strip():
                unknown["scripts"].append(
                    {"source": source_name, "attributes": match.group("attrs"), "body": body}
                )
                continue
            asset = ImportAsset(
                path=f"{source_name}.script{index}.js",
                data=body.encode("utf-8"),
                kind="script",
                executable=False,
                sandbox="none",
            )
            assets.append(asset)
            unknown["scripts"].append(
                {
                    "source": source_name,
                    "asset": asset.to_asset_ref(),
                    "attributes": match.group("attrs"),
                    "executed": False,
                }
            )
            unknown["opaque"].append(f"script:{asset.path}")
        for index, match in enumerate(_STYLE_RE.finditer(text)):
            body = match.group("body") or ""
            if not body:
                continue
            asset = ImportAsset(
                path=f"{source_name}.style{index}.css",
                data=body.encode("utf-8"),
                kind="theme",
                executable=False,
                sandbox="none",
            )
            assets.append(asset)
            unknown["styles"].append(
                {"source": source_name, "asset": asset.to_asset_ref(), "executed": False}
            )
            unknown["opaque"].append(f"style:{asset.path}")
        # A payload whose bytes are not valid UTF-8 is still recoverable.
        if "\ufffd" in text:
            unknown["payload_encoding"] = base64.b64encode(text.encode("utf-8")).decode(
                "ascii"
            )

    # ------------------------------------------------------------------
    # Widget projection
    # ------------------------------------------------------------------
    def _element_widget(
        self,
        element: ElementTree.Element,
        *,
        raw: bytes,
        tag: str,
        kind: str,
        source_format: str,
        source_name: str,
        index_prefix: str,
        index: int,
        seen_widget_ids: set[str],
    ) -> dict[str, Any] | None:
        attributes = dict(element.attrib)
        element_document = _element_to_dict(element)
        metric = _attr(attributes, "sensorid", "sensor", "id", "metric", "name")
        label = _attr(attributes, "label", "text", "caption", "title", "name")
        if metric is None:
            metric = label or f"{tag}-{index}"
        widget_id = self._unique_id(
            f"{index_prefix}aida64-{_slug(str(metric))}", seen_widget_ids
        )
        return self._build_widget(
            widget_id=widget_id,
            raw=raw,
            kind=kind,
            metric=str(metric),
            label=str(label) if label is not None else "",
            attributes=attributes,
            source_format=source_format,
            source_name=source_name,
            tag=tag,
            opaque=element_document,
        )

    def _mapping_widget(
        self,
        item: Mapping[str, Any],
        *,
        raw: bytes,
        tag: str,
        kind: str,
        source_format: str,
        source_name: str,
        index_prefix: str,
        index: int,
        seen_widget_ids: set[str],
    ) -> dict[str, Any] | None:
        metric = (
            item.get("sensorid")
            or item.get("sensor")
            or item.get("id")
            or item.get("metric")
            or item.get("name")
        )
        label = item.get("label") or item.get("text") or item.get("caption") or item.get("title")
        if metric is None:
            metric = label or f"{tag}-{index}"
        widget_id = self._unique_id(
            f"{index_prefix}aida64-{_slug(str(metric))}", seen_widget_ids
        )
        attributes = {str(key): value for key, value in item.items()}
        return self._build_widget(
            widget_id=widget_id,
            raw=raw,
            kind=kind,
            metric=str(metric),
            label=str(label) if label is not None else "",
            attributes=attributes,
            source_format=source_format,
            source_name=source_name,
            tag=tag,
            opaque=dict(item),
        )

    def _build_widget(
        self,
        *,
        widget_id: str,
        raw: bytes,
        kind: str,
        metric: str,
        label: str,
        attributes: Mapping[str, Any],
        source_format: str,
        source_name: str,
        tag: str,
        opaque: Any,
    ) -> dict[str, Any]:
        unit = _attr(attributes, "unit", "units", "suffix")
        binding: dict[str, Any] = {"metric": metric, "source": "aida64"}
        if unit:
            binding["unit"] = str(unit)

        minimum = _number(_attr(attributes, "min", "minimum", "minvalue"))
        maximum = _number(_attr(attributes, "max", "maximum", "maxvalue"))
        domain: dict[str, Any] = {}
        if minimum is not None and maximum is not None and minimum <= maximum:
            domain = {"min": minimum, "max": maximum}
        elif minimum is not None or maximum is not None:
            domain = {}
        nominal = _number(_attr(attributes, "nominal", "nominalvalue"))
        if nominal is not None:
            domain["nominal"] = nominal

        layout: dict[str, Any] = {}
        for key, read_keys in (
            ("x", ("x", "left")),
            ("y", ("y", "top")),
            ("width", ("width", "w")),
            ("height", ("height", "h")),
        ):
            number = _number(_attr(attributes, *read_keys))
            if number is None:
                continue
            if key in {"width", "height"} and number <= 0:
                continue
            layout[key] = number

        tokens: dict[str, Any] = {}
        for key, value in attributes.items():
            lower = _local_name(str(key)).lower()
            if lower in _COLOR_KEYS and value not in (None, ""):
                tokens[lower] = str(value)
        style = {"tokens": tokens} if tokens else None

        extensions = {
            "aida64": {
                "tag": tag,
                "format": source_format,
                "source": source_name,
                "attributes": dict(attributes),
                "element": opaque,
            }
        }
        widget = make_widget(
            widget_id,
            kind,
            source_format,
            raw,
            metric=metric,
            label=label,
            domain=domain or None,
            layout=layout or None,
            style=style,
            extensions=extensions,
        )
        widget["binding"].update(
            {key: value for key, value in binding.items() if key != "metric"}
        )
        return widget

    @staticmethod
    def _unique_id(candidate: str, seen: set[str]) -> str:
        unique = candidate
        index = 1
        while unique in seen:
            index += 1
            unique = f"{candidate}-{index}"
        seen.add(unique)
        return unique


# Compatibility aliases for callers that prefer the vendor capitalisation.
AIDA64Importer = Aida64Importer

__all__ = [
    "AIDA64Importer",
    "Aida64Importer",
    "MAX_COMPRESSION_RATIO",
    "MAX_MEMBER_SIZE",
    "MAX_MEMBERS",
    "MAX_TOTAL_UNCOMPRESSED",
]
