# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Lossless Home Assistant dashboard/card YAML importer.

Only :func:`yaml.safe_load` is used; unsafe tags are rejected and no entity
state is fetched. Known card families are projected into :class:`WidgetIR`,
while custom cards, tap actions, and unknown fields remain opaque extensions.
"""

from __future__ import annotations

import math
import re

from typing import Any, Mapping

import yaml

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

#: Hard cap before YAML parsing.
MAX_SOURCE_BYTES = 4 * 1024 * 1024

_CARD_KINDS = {
    "gauge": "gauge",
    "sensor": "number",
    "entity": "number",
    "history-graph": "chart",
    "history_graph": "chart",
    "statistics-graph": "chart",
    "statistics_graph": "chart",
    "markdown": "text",
    "button": "status",
    "entity-button": "status",
    "entity_button": "status",
    "switch": "status",
    "toggle": "status",
    "picture": "icon",
    "picture-entity": "icon",
    "picture_entity": "icon",
    "picture-glance": "icon",
    "picture_glance": "icon",
    "image": "icon",
    "icon": "icon",
    "bar": "bar",
    "bar-card": "bar",
    "bar_card": "bar",
    "glance": "number",
    "entities": "number",
}

_CONTAINER_TYPES = {
    "entities",
    "glance",
    "grid",
    "vertical-stack",
    "horizontal-stack",
    "vertical_stack",
    "horizontal_stack",
}

_ENTITY_LIST_TYPES = {"entities", "glance"}

_KNOWN_CARD_KEYS = {
    "type",
    "entity",
    "entities",
    "name",
    "title",
    "content",
    "unit",
    "unit_of_measurement",
    "attribute",
    "min",
    "max",
    "limits",
    "severity",
    "hours_to_show",
    "days_to_show",
    "refresh_interval",
    "tap_action",
    "hold_action",
    "double_tap_action",
    "icon",
    "image",
    "image_url",
    "entity_picture",
    "show_name",
    "show_icon",
    "show_state",
    "state_color",
    "columns",
    "square",
    "aspect_ratio",
    "theme",
    "layout",
    "grid_options",
    "view_layout",
    "cards",
    "badges",
    "path",
    "subtitle",
    "footer",
    "url",
    "navigation_path",
    "background",
}

_IMAGE_KEYS = ("image", "image_url", "entity_picture", "background", "url")
_UNSAFE_TAG_RE = re.compile(r"!!python/")


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


def _slug(value: str) -> str:
    slug = re.sub(r"[^A-Za-z0-9_.-]+", "-", value).strip("-")
    return slug or "item"


class HomeAssistantImporter(BaseImporter):
    """Import Home Assistant dashboard or card YAML losslessly."""

    format = "home_assistant_yaml"
    version = "1"
    suffixes = (".yaml", ".yml")
    mime = "application/yaml"

    def sniff(self, data: bytes) -> bool:
        if not data:
            return False
        text = data[:16384].decode("utf-8", errors="ignore")
        if re.search(r"(?m)^\s*views\s*:", text):
            return True
        if re.search(r"(?m)^\s*cards\s*:", text):
            return True
        return re.search(r"(?m)^\s*type\s*:\s*(?:custom:|gauge|sensor|entities)", text) is not None

    def parse(
        self,
        name: str,
        data: bytes,
        context: ImportContext,
    ) -> ImportResult:
        raw = bytes(data)
        if len(raw) > MAX_SOURCE_BYTES:
            raise ImportError(
                f"Home Assistant source is {len(raw)} bytes; limit is {MAX_SOURCE_BYTES}"
            )
        text = raw.decode("utf-8-sig", errors="replace")
        source_path = name or "dashboard.yaml"
        original = ImportAsset(
            path=source_path, data=raw, kind="original", executable=False, sandbox="none"
        )
        warnings: list[str] = []
        unknown: dict[str, Any] = {
            "cards": [],
            "containers": [],
            "custom_cards": [],
            "asset_references": [],
            "opaque": [],
        }

        if _UNSAFE_TAG_RE.search(text):
            raise ImportError("unsafe or unsupported YAML tag rejected")
        try:
            document = yaml.safe_load(text)
        except RecursionError as exc:
            raise ImportError(
                "Home Assistant YAML nesting exceeds the parser limit"
            ) from exc
        except yaml.YAMLError as exc:
            raise ImportError(f"unsafe or invalid Home Assistant YAML: {exc}") from exc
        if document is None:
            raise ImportError("empty Home Assistant YAML document")
        if not isinstance(document, (Mapping, list)):
            raise ImportError("Home Assistant YAML must be a dashboard mapping or card list")

        cards = self._collect_cards(document)
        expanded = list(self._expand_cards(cards, unknown))
        widgets: list[dict[str, Any]] = []
        seen_ids: set[str] = set()
        for index, card in enumerate(expanded):
            widget = self._card_widget(
                card,
                raw=raw,
                index=index,
                unknown=unknown,
                warnings=warnings,
                seen_ids=seen_ids,
            )
            if widget is not None:
                widgets.append(widget)

        if not widgets:
            warnings.append("no Home Assistant cards were mapped; YAML retained")

        parsed = document
        source_document = make_source_document(
            self.format,
            self.version,
            raw,
            original=original,
            parsed=parsed,
            unknown=unknown,
        )
        source_capture = make_source_capture(
            self.format,
            self.version,
            raw,
            assets=[original],
            opaque=list(dict.fromkeys(unknown["opaque"])),
            warnings=warnings,
        )
        extensions = {
            "home_assistant": {
                "card_count": len(expanded),
                "custom_card_count": len(unknown["custom_cards"]),
                "asset_reference_count": len(unknown["asset_references"]),
                "cards": unknown["cards"],
                "containers": unknown["containers"],
                "custom_cards": unknown["custom_cards"],
                "asset_references": unknown["asset_references"],
                "opaque": unknown["opaque"],
                "raw_yaml": text,
            }
        }
        return ImportResult(
            format=self.format,
            version=self.version,
            raw=raw,
            widgets=widgets,
            source_document=source_document,
            source_capture=source_capture,
            assets=[original],
            unknown=unknown,
            warnings=warnings,
            extensions=extensions,
        )

    # ------------------------------------------------------------------
    # Document traversal
    # ------------------------------------------------------------------
    def _collect_cards(self, document: Mapping[str, Any] | list[Any]) -> list[Any]:
        if isinstance(document, list):
            return list(document)
        if not isinstance(document, Mapping):
            raise ImportError("Home Assistant YAML must contain cards")
        if document.get("type") or document.get("entity"):
            return [document]
        views = document.get("views")
        if isinstance(views, list):
            cards: list[Any] = []
            for view in views:
                if not isinstance(view, Mapping):
                    continue
                view_cards = view.get("cards")
                if isinstance(view_cards, list):
                    cards.extend(view_cards)
                elif view.get("type"):
                    cards.append(view)
            return cards
        nested = document.get("cards")
        if isinstance(nested, list):
            return list(nested)
        return [document]

    def _expand_cards(
        self,
        cards: list[Any],
        unknown: dict[str, Any],
        depth: int = 0,
    ) -> list[Mapping[str, Any]]:
        if depth > 32:
            unknown["opaque"].append("card-nesting-limit")
            return []
        expanded: list[Mapping[str, Any]] = []
        for card in cards:
            if isinstance(card, str):
                expanded.append({"type": "entity", "entity": card})
                continue
            if not isinstance(card, Mapping):
                unknown["opaque"].append("non-mapping-card")
                continue
            card_type = str(card.get("type") or "").lower()
            if card_type in _CONTAINER_TYPES:
                unknown["containers"].append(dict(card))
                nested = card.get("cards")
                if isinstance(nested, list):
                    expanded.extend(self._expand_cards(nested, unknown, depth + 1))
                    continue
                entities = card.get("entities")
                if card_type in _ENTITY_LIST_TYPES and isinstance(entities, list):
                    for entry in entities:
                        expanded.append(self._entity_card(entry, card))
                    continue
            expanded.append(dict(card))
        return expanded

    @staticmethod
    def _entity_card(entry: Any, parent: Mapping[str, Any]) -> Mapping[str, Any]:
        if isinstance(entry, str):
            card: dict[str, Any] = {"type": "entity", "entity": entry}
        elif isinstance(entry, Mapping):
            card = dict(entry)
            card.setdefault("type", "entity")
        else:
            card = {"type": "entity"}
        for key in ("name", "title", "unit", "unit_of_measurement", "attribute"):
            if key in parent and key not in card:
                card[key] = parent[key]
        card.setdefault("_container", parent.get("type"))
        return card

    # ------------------------------------------------------------------
    # Widget projection
    # ------------------------------------------------------------------
    def _card_widget(
        self,
        card: Mapping[str, Any],
        *,
        raw: bytes,
        index: int,
        unknown: dict[str, Any],
        warnings: list[str],
        seen_ids: set[str],
    ) -> dict[str, Any] | None:
        card_type = str(card.get("type") or "").lower()
        if not card_type:
            card_type = "entity" if card.get("entity") else "generic"
        custom = card_type.startswith("custom:")
        kind = "custom" if custom else _CARD_KINDS.get(card_type)
        if kind is None:
            kind = "number" if card.get("entity") else "card"
        if custom:
            warnings.append(f"custom card `{card_type}` retained opaque")

        entity = self._first_entity(card)
        attribute = card.get("attribute")
        unit = card.get("unit") or card.get("unit_of_measurement")
        label = (
            card.get("name")
            or card.get("title")
            or card.get("content")
            or entity
            or card_type
        )
        label_text = str(label)
        if card_type == "markdown":
            label_text = label_text[:120]

        metric = str(entity or f"home_assistant.{card_type or 'card'}.{index}")
        binding: dict[str, Any] = {"metric": metric, "source": "home_assistant"}
        if unit:
            binding["unit"] = str(unit)
        if attribute:
            binding["property"] = str(attribute)

        domain: dict[str, Any] = {}
        minimum = _number(card.get("min"))
        maximum = _number(card.get("max"))
        limits = card.get("limits")
        if isinstance(limits, Mapping):
            if minimum is None:
                minimum = _number(limits.get("min"))
            if maximum is None:
                maximum = _number(limits.get("max"))
        if minimum is not None and maximum is not None and minimum <= maximum:
            domain = {"min": minimum, "max": maximum}

        history = self._history_for(card)

        actions: list[dict[str, Any]] = []
        for action_key in ("tap_action", "hold_action", "double_tap_action"):
            action = card.get(action_key)
            if isinstance(action, Mapping):
                actions.append(
                    {
                        "key": f"home_assistant.{action_key}",
                        "label": str(action.get("action") or action_key),
                        "params": dict(action),
                    }
                )

        asset_references = self._asset_references(card)
        unknown["asset_references"].extend(asset_references)
        if custom:
            unknown["custom_cards"].append(
                {"type": card_type, "entity": entity, "config": dict(card), "opaque": True}
            )
            unknown["opaque"].append(f"custom:{card_type}")

        unknown_keys = [
            str(key) for key in card if str(key).lower() not in _KNOWN_CARD_KEYS
        ]
        unknown["cards"].append(
            {
                "type": card_type,
                "entity": entity,
                "config": dict(card),
                "opaque": custom,
                "unknown_keys": unknown_keys,
            }
        )

        extensions = {
            "home_assistant": {
                "type": card_type,
                "card": dict(card),
                "entity": entity or "",
                "attribute": str(attribute) if attribute is not None else "",
                "unit": str(unit) if unit is not None else "",
                "tap_action": card.get("tap_action"),
                "hold_action": card.get("hold_action"),
                "double_tap_action": card.get("double_tap_action"),
                "unknown_keys": unknown_keys,
                "custom_card": custom,
                "opaque": custom,
                "executable": False,
                "asset_references": asset_references,
            }
        }

        widget_id = self._unique_id(
            f"home-assistant-{_slug(entity or card_type or 'card')}-{index}",
            seen_ids,
        )
        widget = make_widget(
            widget_id,
            kind,
            self.format,
            raw,
            metric=metric,
            label=label_text,
            domain=domain or None,
            style=self._style_for(card),
            extensions=extensions,
        )
        widget["binding"].update(
            {key: value for key, value in binding.items() if key != "metric"}
        )
        # ``make_widget`` covers the common IR fields; history and actions are
        # optional IR members without helper parameters.
        if history:
            widget["history"] = history
        if actions:
            widget["actions"] = actions
        return widget

    @staticmethod
    def _first_entity(card: Mapping[str, Any]) -> str:
        entity = card.get("entity")
        if isinstance(entity, str) and entity.strip():
            return entity.strip()
        entities = card.get("entities")
        if isinstance(entities, list):
            for entry in entities:
                if isinstance(entry, str) and entry.strip():
                    return entry.strip()
                if isinstance(entry, Mapping) and isinstance(entry.get("entity"), str):
                    return entry["entity"].strip()
        return ""

    @staticmethod
    def _history_for(card: Mapping[str, Any]) -> dict[str, Any] | None:
        history: dict[str, Any] = {}
        hours = _number(card.get("hours_to_show"))
        days = _number(card.get("days_to_show"))
        if hours is not None and hours >= 0:
            history["window_seconds"] = hours * 3600
        elif days is not None and days >= 0:
            history["window_seconds"] = days * 86400
        refresh = _number(card.get("refresh_interval"))
        if refresh is not None and refresh > 0:
            history["sample_rate_hz"] = 1.0 / refresh
        return history or None

    @staticmethod
    def _style_for(card: Mapping[str, Any]) -> dict[str, Any] | None:
        tokens: dict[str, Any] = {}
        for key in ("theme", "icon", "image", "background"):
            value = card.get(key)
            if value is not None:
                tokens[key] = value
        return {"tokens": tokens} if tokens else None

    @staticmethod
    def _asset_references(card: Mapping[str, Any]) -> list[dict[str, str]]:
        references: list[dict[str, str]] = []
        for key in _IMAGE_KEYS:
            value = card.get(key)
            if not isinstance(value, str) or not value.strip():
                continue
            if value.startswith(("http://", "https://", "mdi:", "hass:")):
                continue
            references.append({"key": key, "value": value})
        return references

    @staticmethod
    def _unique_id(candidate: str, seen: set[str]) -> str:
        unique = candidate
        index = 1
        while unique in seen:
            index += 1
            unique = f"{candidate}-{index}"
        seen.add(unique)
        return unique


__all__ = ["HAImporter", "HomeAssistantImporter"]

# Compatibility alias for callers that abbreviate Home Assistant.
HAImporter = HomeAssistantImporter
