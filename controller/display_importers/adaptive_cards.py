# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Lossless Adaptive Cards 1.x import and card-subset export.

Import keeps the complete card document and every unsupported element or
action verbatim in ``extensions``.  The typed projection covers TextBlock,
FactSet, Image, Media, Container, ColumnSet, Column, and ActionSet elements.

Export produces an Adaptive Card 1.5 document for the supported subset and a
``TranslationReport``.  Gauges/charts are approximated as text; executable
fields and unsafe actions are stripped and reported as dropped.
"""

from __future__ import annotations

import json
import re
from typing import Any, Iterator, Mapping

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

FORMAT = "adaptive_card"
EXPORT_VERSION = "1.5"
SCHEMA_URL = "http://adaptivecards.io/schemas/adaptive-card.json"
TRANSLATOR = "controller.display_importers.adaptive_cards"
TRANSLATOR_VERSION = "1.0.0"

MAX_SOURCE_BYTES = 2_097_152
MAX_IMPORT_DEPTH = 64
MAX_EXPORT_WIDGETS = 200
MAX_EXPORT_BYTES = 1_048_576
MAX_EXPORT_DEPTH = 8

_ELEMENT_KINDS = {
    "TextBlock": "text",
    "FactSet": "table",
    "Image": "icon",
    "Media": "card",
    "Container": "card",
    "ColumnSet": "card",
    "Column": "card",
    "ActionSet": "card",
}

_EXPORT_ELEMENT_TYPES = frozenset(
    {
        "TextBlock",
        "FactSet",
        "Image",
        "Media",
        "Container",
        "ColumnSet",
        "Column",
        "ActionSet",
    }
)

_KNOWN_ACTION_TYPES = frozenset(
    {
        "Action.OpenUrl",
        "Action.Submit",
        "Action.Execute",
        "Action.ShowCard",
        "Action.ToggleVisibility",
    }
)

_EXPORT_SAFE_ACTIONS = frozenset({"Action.OpenUrl"})

_CONTAINER_KEYS = {
    "Container": "items",
    "Column": "items",
    "ColumnSet": "columns",
}

_UNSAFE_KEY_RE = re.compile(
    r"^(?:on[a-z]+|script|javascript|execute|onexecute|selector)$", re.IGNORECASE
)
_UNSAFE_URL_PREFIXES = ("javascript:", "data:", "vbscript:")
_DROP = object()


def _slug(value: str) -> str:
    slug = re.sub(r"[^a-z0-9]+", "-", value.lower()).strip("-")
    return slug or "element"


class AdaptiveCardError(ImportError):
    """An Adaptive Card cannot be read or exported within the bounds."""


class AdaptiveCardExport(dict[str, Any]):
    """An exported card plus its report.

    The object is a mapping of the card itself, so it can be serialized or
    indexed like the card, while also supporting attribute access
    (``.card``/``.report``), mapping access (``["card"]``/
    ``["translation_report"]``), and tuple unpacking for callers that prefer
    ``card, report = export_adaptive_card(...)``.
    """

    def __init__(
        self, card: dict[str, Any], report: dict[str, Any]
    ) -> None:
        super().__init__(card)
        self._card = card
        self.report = report

    @property
    def card(self) -> dict[str, Any]:
        return self._card

    @property
    def translation_report(self) -> dict[str, Any]:
        return self.report

    def __getitem__(self, key: Any) -> Any:
        if key in {"card", 0}:
            return self._card
        if key in {"report", "translation_report", 1}:
            return self.report
        return super().__getitem__(key)

    def get(self, key: Any, default: Any = None) -> Any:
        try:
            return self[key]
        except (KeyError, TypeError):
            return default

    def __contains__(self, key: Any) -> bool:
        return (
            key in {"card", "report", "translation_report", 0, 1}
            or super().__contains__(key)
        )

    def __iter__(self) -> Iterator[dict[str, Any]]:
        yield self._card
        yield self.report

    def to_dict(self) -> dict[str, Any]:
        return {
            "card": self._card,
            "translation_report": self.report,
        }


def _load_json(data: bytes) -> Any:
    try:
        text = data.decode("utf-8-sig")
        return json.loads(text)
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise AdaptiveCardError(f"invalid Adaptive Card JSON: {exc}") from exc


def _safe_url(value: Any) -> str:
    if not isinstance(value, str) or not value.strip():
        return ""
    url = value.strip()
    if url.lower().startswith(_UNSAFE_URL_PREFIXES):
        return ""
    return url


def _sanitize_value(
    value: Any,
    *,
    depth: int,
    max_depth: int,
    dropped: list[str],
    path: str,
) -> Any:
    """Recursively strip script/executable fields from a raw card fragment."""
    if depth > max_depth:
        dropped.append(f"{path}: exceeded export nesting limit")
        return _DROP
    if isinstance(value, Mapping):
        element_type = value.get("type")
        if isinstance(element_type, str):
            if element_type.startswith("Action."):
                if element_type != "Action.OpenUrl":
                    dropped.append(f"{path}: executable action `{element_type}`")
                    return _DROP
                if not _safe_url(value.get("url")):
                    dropped.append(f"{path}: unsafe action URL")
                    return _DROP
            elif element_type not in _EXPORT_ELEMENT_TYPES:
                dropped.append(f"{path}: unsupported element `{element_type}`")
                return _DROP
        result: dict[str, Any] = {}
        for key, item in value.items():
            key_text = str(key)
            child_path = f"{path}.{key_text}"
            if _UNSAFE_KEY_RE.match(key_text):
                dropped.append(f"{child_path}: executable field")
                continue
            if key_text == "selectAction":
                sanitized = _sanitize_value(
                    item,
                    depth=depth + 1,
                    max_depth=max_depth,
                    dropped=dropped,
                    path=child_path,
                )
                if sanitized is _DROP:
                    continue
                result[key_text] = sanitized
                continue
            sanitized = _sanitize_value(
                item,
                depth=depth + 1,
                max_depth=max_depth,
                dropped=dropped,
                path=child_path,
            )
            if sanitized is _DROP:
                continue
            result[key_text] = sanitized
        if element_type == "ActionSet" and not result.get("actions"):
            dropped.append(f"{path}: ActionSet had no safe actions")
            return _DROP
        if element_type == "ColumnSet" and not result.get("columns"):
            dropped.append(f"{path}: ColumnSet had no safe columns")
            return _DROP
        if element_type == "Image" and not _safe_url(result.get("url")):
            dropped.append(f"{path}: Image has no safe URL")
            return _DROP
        if element_type == "Media":
            sources = result.get("sources")
            if isinstance(sources, list):
                result["sources"] = [
                    source
                    for source in sources
                    if isinstance(source, Mapping)
                    and _safe_url(source.get("url"))
                ]
            if not result.get("sources"):
                dropped.append(f"{path}: Media has no retained sources")
                return _DROP
        if element_type == "TextBlock" and not isinstance(result.get("text"), str):
            dropped.append(f"{path}: TextBlock has no text")
            return _DROP
        if element_type == "FactSet":
            facts = result.get("facts")
            cleaned_facts: list[dict[str, str]] = []
            if isinstance(facts, list):
                for fact in facts:
                    if not isinstance(fact, Mapping):
                        continue
                    title = fact.get("title")
                    value = fact.get("value")
                    if isinstance(title, str) and isinstance(value, str):
                        cleaned_facts.append({"title": title, "value": value})
                    else:
                        dropped.append(f"{path}: FactSet fact without title/value")
            if not cleaned_facts:
                dropped.append(f"{path}: FactSet has no facts")
                return _DROP
            result["facts"] = cleaned_facts
        return result
    if isinstance(value, list):
        result_list: list[Any] = []
        for index, item in enumerate(value):
            sanitized = _sanitize_value(
                item,
                depth=depth + 1,
                max_depth=max_depth,
                dropped=dropped,
                path=f"{path}[{index}]",
            )
            if sanitized is not _DROP:
                result_list.append(sanitized)
        return result_list
    return value


def _adaptive_extension(widget: Mapping[str, Any]) -> Mapping[str, Any]:
    extensions = widget.get("extensions")
    if not isinstance(extensions, Mapping):
        return {}
    for key in ("adaptive_card", "adaptive"):
        value = extensions.get(key)
        if isinstance(value, Mapping):
            return value
    return {}


def _widget_label(widget: Mapping[str, Any]) -> str:
    label = widget.get("label")
    if isinstance(label, str) and label.strip():
        return label
    binding = widget.get("binding")
    if isinstance(binding, Mapping):
        metric = binding.get("metric")
        if isinstance(metric, str) and metric.strip():
            return metric
    return str(widget.get("id") or "widget")


def _facts(widget: Mapping[str, Any]) -> list[dict[str, str]]:
    adaptive = _adaptive_extension(widget)
    candidates: list[Any] = []
    for source in (adaptive, widget):
        for key in ("facts", "rows"):
            value = source.get(key)
            if isinstance(value, list):
                candidates.append(value)
    for candidate in candidates:
        facts: list[dict[str, str]] = []
        for item in candidate:
            if isinstance(item, Mapping):
                title = item.get("title", item.get("label", ""))
                value = item.get("value", item.get("metric", ""))
                if title or value:
                    facts.append(
                        {"title": str(title or value), "value": str(value or title)}
                    )
            elif isinstance(item, (list, tuple)) and len(item) == 2:
                facts.append({"title": str(item[0]), "value": str(item[1])})
        if facts:
            return facts
    return []


def _image_url(widget: Mapping[str, Any]) -> str:
    adaptive = _adaptive_extension(widget)
    element = adaptive.get("element")
    if isinstance(element, Mapping):
        url = _safe_url(element.get("url"))
        if url:
            return url
    for source in (adaptive, widget):
        for key in ("url", "src", "image", "image_url"):
            url = _safe_url(source.get(key))
            if url:
                return url
    return ""


def _layout_widget(item: Mapping[str, Any]) -> dict[str, Any]:
    reverse = {"label": "text", "sparkline": "chart", "icon": "status"}
    kind = item.get("ir_kind") or reverse.get(
        str(item.get("type") or ""), item.get("type") or "custom"
    )
    binding = item.get("binding")
    if not isinstance(binding, Mapping):
        metric = item.get("metric") or item.get("id") or "widget"
        binding = {"metric": str(metric)}
    widget = {
        "id": str(item.get("ir_id") or item.get("id") or "widget"),
        "kind": str(kind),
        "binding": dict(binding),
    }
    for key in ("label", "extensions", "domain", "layout", "typography"):
        if key in item:
            widget[key] = item[key]
    return widget


def _normalize_widgets(widgets_or_layout: Any) -> list[dict[str, Any]]:
    if isinstance(widgets_or_layout, Mapping):
        if isinstance(widgets_or_layout.get("widgets"), list):
            items = widgets_or_layout["widgets"]
        elif isinstance(widgets_or_layout.get("layout"), Mapping) and isinstance(
            widgets_or_layout["layout"].get("widgets"), list
        ):
            items = widgets_or_layout["layout"]["widgets"]
        elif isinstance(widgets_or_layout.get("ir"), Mapping):
            items = [widgets_or_layout["ir"]]
        else:
            items = [widgets_or_layout]
    elif isinstance(widgets_or_layout, (list, tuple)):
        items = list(widgets_or_layout)
    else:
        raise AdaptiveCardError("widgets_or_layout must be widgets or a layout")
    widgets: list[dict[str, Any]] = []
    for item in items:
        if not isinstance(item, Mapping):
            raise AdaptiveCardError("every widget must be an object")
        if "kind" in item and "binding" in item:
            widgets.append(dict(item))
        else:
            widgets.append(_layout_widget(item))
    return widgets


def _text_block(text: str) -> dict[str, Any]:
    return {"type": "TextBlock", "text": text, "wrap": True}


def _element_for_widget(
    widget: Mapping[str, Any],
    *,
    converted: list[str],
    approximated: list[str],
    dropped: list[str],
    max_depth: int,
) -> dict[str, Any]:
    widget_id = str(widget.get("id") or "widget")
    kind = str(widget.get("kind") or "custom")
    label = _widget_label(widget)
    adaptive = _adaptive_extension(widget)
    raw = adaptive.get("element")
    if isinstance(raw, Mapping):
        sanitized = _sanitize_value(
            raw,
            depth=0,
            max_depth=max_depth,
            dropped=dropped,
            path=widget_id,
        )
        if sanitized is not _DROP and isinstance(sanitized, Mapping):
            converted.append(widget_id)
            return dict(sanitized)

    if kind == "text" or kind == "status":
        converted.append(widget_id)
        return _text_block(label)
    if kind == "table":
        facts = _facts(widget)
        if facts:
            converted.append(widget_id)
            return {"type": "FactSet", "facts": facts}
        approximated.append(f"{widget_id}: table exported as TextBlock")
        return _text_block(label)
    if kind == "icon":
        url = _image_url(widget)
        if url:
            converted.append(widget_id)
            return {"type": "Image", "url": url, "altText": label}
        approximated.append(f"{widget_id}: icon exported as TextBlock")
        dropped.append(f"{widget_id}: image source")
        return _text_block(label)
    if kind == "card":
        converted.append(widget_id)
        return {
            "type": "Container",
            "items": [_text_block(label)],
        }
    if kind == "number":
        approximated.append(f"{widget_id}: numeric value exported as TextBlock")
        return _text_block(label)
    if kind in {"gauge", "bar", "sparkline", "chart"}:
        approximated.append(f"{widget_id}: {kind} exported as TextBlock")
        dropped.append(f"{widget_id}: {kind} chart/gauge semantics")
        return _text_block(label)
    approximated.append(f"{widget_id}: {kind} exported as TextBlock")
    return _text_block(label)


def export_adaptive_card(
    widgets_or_layout: Any,
    options: Mapping[str, Any] | None = None,
) -> AdaptiveCardExport:
    """Export widgets/layout as an Adaptive Card 1.5 plus a report.

    ``options`` may set ``max_widgets``, ``max_bytes``, ``max_depth``, and
    ``include_schema``.  Oversized or over-deep exports are rejected.
    """
    options = dict(options or {})
    max_widgets = int(options.get("max_widgets", MAX_EXPORT_WIDGETS))
    max_bytes = int(options.get("max_bytes", MAX_EXPORT_BYTES))
    max_depth = int(options.get("max_depth", MAX_EXPORT_DEPTH))
    if max_widgets <= 0 or max_bytes <= 0 or max_depth <= 0:
        raise AdaptiveCardError("export limits must be positive")
    version = str(options.get("version", EXPORT_VERSION))
    if version != EXPORT_VERSION:
        raise AdaptiveCardError("only Adaptive Card version 1.5 is supported")

    widgets = _normalize_widgets(widgets_or_layout)
    if len(widgets) > max_widgets:
        raise AdaptiveCardError(
            f"export contains {len(widgets)} widgets; limit is {max_widgets}"
        )

    converted: list[str] = []
    approximated: list[str] = []
    dropped: list[str] = []
    warnings: list[str] = []
    body: list[dict[str, Any]] = []
    for widget in widgets:
        element = _element_for_widget(
            widget,
            converted=converted,
            approximated=approximated,
            dropped=dropped,
            max_depth=max_depth,
        )
        body.append(element)

    card: dict[str, Any] = {"type": "AdaptiveCard", "version": EXPORT_VERSION}
    if options.get("include_schema", True):
        card["$schema"] = SCHEMA_URL
    card["body"] = body

    encoded = json.dumps(card, ensure_ascii=False, separators=(",", ":")).encode(
        "utf-8"
    )
    if len(encoded) > max_bytes:
        raise AdaptiveCardError(
            f"exported card is {len(encoded)} bytes; limit is {max_bytes}"
        )

    fidelity = "approximate" if approximated or dropped else "semantic"
    if not body:
        warnings.append("no widgets were exported; the card body is empty")
    report: dict[str, Any] = {
        "direction": "export",
        "source_format": "org.ozma.widget-ir/1",
        "target_format": "adaptive_card",
        "translator": TRANSLATOR,
        "translator_version": TRANSLATOR_VERSION,
        "fidelity": fidelity,
        "converted": converted,
        "approximated": approximated,
        "dropped": dropped,
        "warnings": warnings,
    }
    # Local import mirrors the controller importers' package setup.
    try:
        import display_ir  # type: ignore[import-not-found]
    except ImportError:  # pragma: no cover - controller path is normally present
        pass
    else:
        display_ir.validate_translation_report(report)
    return AdaptiveCardExport(card=card, report=report)


class AdaptiveCardsImporter(BaseImporter):
    """Import Adaptive Cards JSON losslessly."""

    format = FORMAT
    version = ""
    suffixes = (".json",)
    mime = "application/json"

    def sniff(self, data: bytes) -> bool:
        lowered = data.lower()
        return b'"adaptivecard"' in lowered or b"adaptivecards.io/schemas" in lowered

    def _asset(self, name: str, raw: bytes) -> ImportAsset:
        return ImportAsset(
            name or "adaptive_card.json",
            raw,
            mime="application/json",
            kind="original",
            executable=False,
            sandbox="none",
        )

    def parse(
        self,
        name: str,
        data: bytes,
        context: ImportContext,
    ) -> ImportResult:
        if len(data) > MAX_SOURCE_BYTES:
            raise AdaptiveCardError(
                f"Adaptive Card source exceeds {MAX_SOURCE_BYTES} bytes"
            )
        document = _load_json(data)
        if not isinstance(document, Mapping):
            raise AdaptiveCardError("Adaptive Card source must be a JSON object")
        root_type = document.get("type")
        if root_type != "AdaptiveCard":
            raise AdaptiveCardError("root `type` must be `AdaptiveCard`")
        asset = self._asset(name, data)
        version = str(document.get("version") or "")
        warnings: list[str] = []
        if version and not version.startswith("1."):
            warnings.append(
                f"Adaptive Card version `{version}` is outside the 1.x subset; "
                "unknown fields were retained"
            )

        widgets: list[dict[str, Any]] = []
        unsupported: list[dict[str, Any]] = []
        unsupported_actions: list[dict[str, Any]] = []
        counter = 0
        action_types: list[str] = []

        def add_action_audit(actions: Any, path: list[Any]) -> None:
            if not isinstance(actions, list):
                return
            for index, action in enumerate(actions):
                action_path = [*path, index]
                if not isinstance(action, Mapping):
                    unsupported.append(
                        {
                            "path": action_path,
                            "type": None,
                            "element": action,
                            "reason": "action is not an object",
                        }
                    )
                    continue
                action_type = str(action.get("type") or "")
                action_types.append(action_type)
                if action_type and action_type not in _KNOWN_ACTION_TYPES:
                    unsupported.append(
                        {
                            "path": action_path,
                            "type": action_type,
                            "element": dict(action),
                            "reason": "unsupported action",
                        }
                    )
                if action_type and action_type not in _EXPORT_SAFE_ACTIONS:
                    unsupported_actions.append(
                        {
                            "path": action_path,
                            "type": action_type,
                            "element": dict(action),
                            "reason": "retained verbatim; not in the export subset",
                        }
                    )

        def walk(elements: Any, path: list[Any], depth: int = 0) -> None:
            nonlocal counter
            if not isinstance(elements, list):
                return
            if depth > MAX_IMPORT_DEPTH:
                warnings.append(
                    f"Adaptive Card nesting at {path} exceeds {MAX_IMPORT_DEPTH}; "
                    "deeper elements retained opaque"
                )
                return
            for index, element in enumerate(elements):
                element_path = [*path, index]
                if not isinstance(element, Mapping):
                    unsupported.append(
                        {
                            "path": element_path,
                            "type": None,
                            "element": element,
                            "reason": "element is not an object",
                        }
                    )
                    warnings.append(
                        f"Adaptive Card element at {element_path} is not an object"
                    )
                    continue
                element_type = str(element.get("type") or "")
                kind = _ELEMENT_KINDS.get(element_type)
                if kind is None:
                    unsupported.append(
                        {
                            "path": element_path,
                            "type": element_type,
                            "element": dict(element),
                            "reason": "unsupported element",
                        }
                    )
                    warnings.append(
                        f"Adaptive Card element `{element_type or 'untyped'}` at "
                        f"{element_path} retained opaque without a widget"
                    )
                    continue

                widget_id = f"adaptive-{counter}-{_slug(element_type)}"
                counter += 1
                label = ""
                if element_type == "TextBlock":
                    label = str(element.get("text") or "")
                elif element_type == "FactSet":
                    label = str(element.get("title") or "Facts")
                elif element_type == "Image":
                    label = str(element.get("altText") or element.get("url") or "Image")
                elif element_type == "Media":
                    label = str(element.get("altText") or "Media")
                elif element_type in {"Container", "ColumnSet", "Column"}:
                    label = str(element.get("id") or element_type)
                elif element_type == "ActionSet":
                    label = str(element.get("id") or "Actions")
                extensions = {
                    "adaptive_card": {
                        "element": dict(element),
                        "path": element_path,
                        "element_type": element_type,
                    }
                }
                adaptive_ext = extensions["adaptive_card"]
                if element_type == "TextBlock":
                    adaptive_ext["text"] = element.get("text")
                elif element_type == "FactSet":
                    adaptive_ext["facts"] = element.get("facts")
                elif element_type == "Image":
                    adaptive_ext["url"] = element.get("url")
                elif element_type == "Media":
                    adaptive_ext["sources"] = element.get("sources")
                elif element_type == "ActionSet":
                    adaptive_ext["actions"] = element.get("actions")
                widgets.append(
                    make_widget(
                        widget_id,
                        kind,
                        FORMAT,
                        data,
                        metric=f"{element_type.lower()}.{counter - 1}",
                        role=element_type,
                        label=label,
                        extensions=extensions,
                        source_version=version,
                        source_unknown=dict(element),
                    )
                )
                if element_type in _CONTAINER_KEYS:
                    walk(
                        element.get(_CONTAINER_KEYS[element_type]),
                        [*element_path, _CONTAINER_KEYS[element_type]],
                        depth + 1,
                    )
                elif element_type == "ActionSet":
                    add_action_audit(
                        element.get("actions"), [*element_path, "actions"]
                    )

        walk(document.get("body"), ["body"])
        root_actions = document.get("actions")
        if isinstance(root_actions, list):
            add_action_audit(root_actions, ["actions"])
            synthetic = {"type": "ActionSet", "actions": root_actions}
            walk([synthetic], ["actions"], 0)

        unknown = {
            "adaptive_card": {
                "unsupported": unsupported,
                "unsupported_elements": unsupported,
                "unsupported_actions": unsupported_actions,
                "action_types": action_types,
                "executed": False,
            }
        }
        extensions = {
            "adaptive_card": {
                "document": dict(document),
                "version": version,
                "schema": document.get("$schema"),
                "unsupported": unsupported,
                "unsupported_elements": unsupported,
                "unsupported_actions": unsupported_actions,
                "action_types": action_types,
                "executed": False,
                "context": {"name": context.name, "base_dir": context.base_dir},
            }
        }
        source_document = make_source_document(
            FORMAT,
            version,
            data,
            original=asset,
            parsed=dict(document),
            unknown=unknown,
        )
        source_capture = make_source_capture(
            FORMAT,
            version,
            data,
            assets=[asset],
            captured=True,
            opaque=unknown,
            warnings=warnings,
        )
        return ImportResult(
            format=FORMAT,
            version=version,
            raw=data,
            widgets=widgets,
            source_document=source_document,
            source_capture=source_capture,
            assets=[asset],
            unknown=unknown,
            warnings=warnings,
            extensions=extensions,
        )


#: Convenience alias for the singular spelling used in some integration code.
AdaptiveCardImporter = AdaptiveCardsImporter
