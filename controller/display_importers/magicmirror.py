# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Lossless MagicMirror ``config.js`` importer.

MagicMirror configurations are JavaScript.  This importer never evaluates the
source.  It extracts the object assigned to ``config`` with a bounded,
non-evaluating JSON5-like reader (comments, unquoted keys, single-quoted
strings, and trailing commas are accepted).  If the object cannot be recovered
within the parser limits the complete source is retained opaquely and no
widgets are projected.

Known core modules are mapped to :class:`WidgetIR`; community ``MMM-*`` modules
are mapped best-effort to card widgets.  Every original module object, CSS
reference, and asset path is retained in ``extensions``.
"""

from __future__ import annotations

import re
from typing import Any, Mapping

from .base import (
    BaseImporter,
    ImportAsset,
    ImportContext,
    ImportResult,
    make_source_capture,
    make_source_document,
    make_widget,
)

FORMAT = "magicmirror_js"
VERSION = "config.js"

#: Hard limits for the non-evaluating config reader.
MAX_SOURCE_BYTES = 1_048_576
MAX_DEPTH = 64
MAX_NODES = 200_000

_MODULE_KINDS = {
    "clock": "text",
    "currentweather": "icon",
    "weatherforecast": "table",
    "calendar": "table",
    "newsfeed": "table",
    "compliments": "text",
    "alert": "card",
    "updatenotification": "text",
}

_ASSET_KEY_PARTS = (
    "css",
    "stylesheet",
    "style",
    "image",
    "img",
    "logo",
    "icon",
    "font",
    "asset",
    "background",
    "media",
    "video",
    "audio",
    "url",
    "src",
    "endpoint",
    "href",
)

_IMAGE_SUFFIXES = (".png", ".jpg", ".jpeg", ".gif", ".svg", ".webp", ".avif")
_FONT_SUFFIXES = (".ttf", ".otf", ".woff", ".woff2")
_MEDIA_SUFFIXES = (".mp4", ".webm", ".mp3", ".wav", ".ogg", ".m4a")

_NUMBER_RE = re.compile(
    r"[+-]?(?:0[xX][0-9a-fA-F]+|(?:\d+(?:\.\d*)?|\.\d+)"
    r"(?:[eE][+-]?\d+)?|Infinity|NaN)"
)
_IDENTIFIER_RE = re.compile(r"[A-Za-z_$][A-Za-z0-9_$-]*")


class ConfigParseError(ValueError):
    """The bounded, non-evaluating ``config.js`` reader rejected the source."""


class _Parser:
    """A tiny JSON5-like reader with explicit byte/depth/node limits."""

    def __init__(
        self,
        text: str,
        start: int,
        *,
        max_depth: int = MAX_DEPTH,
        max_nodes: int = MAX_NODES,
    ) -> None:
        self.text = text
        self.pos = start
        self.length = len(text)
        self.max_depth = max_depth
        self.max_nodes = max_nodes
        self.nodes = 0

    # -- lexical helpers -------------------------------------------------
    def _skip(self) -> None:
        while self.pos < self.length:
            char = self.text[self.pos]
            if char.isspace():
                self.pos += 1
                continue
            if char == "/" and self.pos + 1 < self.length:
                following = self.text[self.pos + 1]
                if following == "/":
                    end = self.text.find("\n", self.pos + 2)
                    self.pos = self.length if end < 0 else end + 1
                    continue
                if following == "*":
                    end = self.text.find("*/", self.pos + 2)
                    if end < 0:
                        raise ConfigParseError("unterminated block comment")
                    self.pos = end + 2
                    continue
            break

    def _peek(self) -> str:
        return self.text[self.pos] if self.pos < self.length else ""

    def _count_node(self) -> None:
        self.nodes += 1
        if self.nodes > self.max_nodes:
            raise ConfigParseError("config exceeds the parser node limit")

    def _expect(self, token: str) -> None:
        self._skip()
        if not self.text.startswith(token, self.pos):
            raise ConfigParseError(f"expected `{token}` at offset {self.pos}")
        self.pos += len(token)

    # -- values ----------------------------------------------------------
    def parse_value(self, depth: int = 0) -> Any:
        if depth > self.max_depth:
            raise ConfigParseError("config exceeds the parser depth limit")
        self._count_node()
        self._skip()
        char = self._peek()
        if char == "{":
            return self._parse_object(depth + 1)
        if char == "[":
            return self._parse_array(depth + 1)
        if char in {"'", '"', "`"}:
            return self._parse_string(char)
        if char and (char.isdigit() or char in "+-."):
            return self._parse_number()
        if char and (char.isalpha() or char in "_$"):
            return self._parse_identifier()
        raise ConfigParseError(f"unexpected token at offset {self.pos}")

    def _parse_object(self, depth: int) -> dict[str, Any]:
        if depth > self.max_depth:
            raise ConfigParseError("config exceeds the parser depth limit")
        self._expect("{")
        result: dict[str, Any] = {}
        self._skip()
        if self._peek() == "}":
            self.pos += 1
            return result
        while True:
            self._skip()
            key = self._parse_key()
            self._expect(":")
            result[key] = self.parse_value(depth)
            self._skip()
            char = self._peek()
            if char == ",":
                self.pos += 1
                self._skip()
                if self._peek() == "}":
                    self.pos += 1
                    break
                continue
            if char == "}":
                self.pos += 1
                break
            raise ConfigParseError(f"expected `,` or `}}` at offset {self.pos}")
        return result

    def _parse_array(self, depth: int) -> list[Any]:
        if depth > self.max_depth:
            raise ConfigParseError("config exceeds the parser depth limit")
        self._expect("[")
        result: list[Any] = []
        self._skip()
        if self._peek() == "]":
            self.pos += 1
            return result
        while True:
            result.append(self.parse_value(depth))
            self._skip()
            char = self._peek()
            if char == ",":
                self.pos += 1
                self._skip()
                if self._peek() == "]":
                    self.pos += 1
                    break
                continue
            if char == "]":
                self.pos += 1
                break
            raise ConfigParseError(f"expected `,` or `]` at offset {self.pos}")
        return result

    def _parse_key(self) -> str:
        self._skip()
        char = self._peek()
        if char in {"'", '"', "`"}:
            return self._parse_string(char)
        if char and (char.isalpha() or char in "_$"):
            match = _IDENTIFIER_RE.match(self.text, self.pos)
            if not match:
                raise ConfigParseError(f"invalid object key at offset {self.pos}")
            self.pos = match.end()
            return match.group(0)
        if char.isdigit():
            return str(self._parse_number())
        raise ConfigParseError(f"invalid object key at offset {self.pos}")

    def _parse_string(self, quote: str) -> str:
        start = self.pos
        self.pos += 1
        pieces: list[str] = []
        while self.pos < self.length:
            char = self.text[self.pos]
            if char == quote:
                self.pos += 1
                return "".join(pieces)
            if char == "\\":
                self.pos += 1
                if self.pos >= self.length:
                    break
                pieces.append(self._parse_escape())
                continue
            if char in {"\n", "\r"} and quote != "`":
                raise ConfigParseError(
                    f"unterminated string starting at offset {start}"
                )
            if quote == "`" and char == "$" and self.text.startswith("${", self.pos):
                raise ConfigParseError("template interpolation is not supported")
            pieces.append(char)
            self.pos += 1
        raise ConfigParseError(f"unterminated string starting at offset {start}")

    def _parse_escape(self) -> str:
        char = self.text[self.pos]
        self.pos += 1
        simple = {
            "b": "\b",
            "f": "\f",
            "n": "\n",
            "r": "\r",
            "t": "\t",
            "v": "\v",
            "0": "\0",
            "\n": "",
            "\r": "",
        }
        if char in simple:
            if char == "\r" and self._peek() == "\n":
                self.pos += 1
            return simple[char]
        if char == "x":
            digits = self.text[self.pos : self.pos + 2]
            if len(digits) != 2 or not all(_is_hex(item) for item in digits):
                raise ConfigParseError("invalid \\x escape")
            self.pos += 2
            return chr(int(digits, 16))
        if char == "u":
            if self.text.startswith("{", self.pos):
                end = self.text.find("}", self.pos + 1)
                if end < 0:
                    raise ConfigParseError("invalid \\u escape")
                digits = self.text[self.pos + 1 : end]
                if not digits or not all(_is_hex(item) for item in digits):
                    raise ConfigParseError("invalid \\u escape")
                self.pos = end + 1
                return chr(int(digits, 16))
            digits = self.text[self.pos : self.pos + 4]
            if len(digits) != 4 or not all(_is_hex(item) for item in digits):
                raise ConfigParseError("invalid \\u escape")
            self.pos += 4
            return chr(int(digits, 16))
        # JavaScript treats unknown escapes as the escaped character.
        return char

    def _parse_number(self) -> int | float:
        match = _NUMBER_RE.match(self.text, self.pos)
        if not match:
            raise ConfigParseError(f"invalid number at offset {self.pos}")
        token = match.group(0)
        end = match.end()
        if end < self.length and (
            self.text[end].isalnum() or self.text[end] in "_$."
        ):
            raise ConfigParseError(f"invalid number token at offset {self.pos}")
        self.pos = end
        lowered = token.lower()
        if "infinity" in lowered or "nan" in lowered:
            raise ConfigParseError("non-finite numbers are not supported")
        sign = -1 if token.startswith("-") else 1
        unsigned = token.lstrip("+-")
        if unsigned.lower().startswith("0x"):
            return sign * int(unsigned, 16)
        if "." in unsigned or "e" in unsigned.lower():
            return sign * float(unsigned)
        return sign * int(unsigned, 10)

    def _parse_identifier(self) -> Any:
        match = _IDENTIFIER_RE.match(self.text, self.pos)
        if not match:
            raise ConfigParseError(f"invalid identifier at offset {self.pos}")
        token = match.group(0)
        self.pos = match.end()
        if token == "true":
            return True
        if token == "false":
            return False
        if token == "null":
            return None
        if token == "undefined":
            return None
        raise ConfigParseError(
            f"identifier `{token}` cannot be evaluated without executing JavaScript"
        )


def _is_hex(char: str) -> bool:
    return char in "0123456789abcdefABCDEF"


def _skip_js(text: str, pos: int) -> int:
    """Skip whitespace, comments, and string literals during scanning."""
    length = len(text)
    while pos < length:
        char = text[pos]
        if char.isspace():
            pos += 1
            continue
        if char == "/" and pos + 1 < length:
            following = text[pos + 1]
            if following == "/":
                end = text.find("\n", pos + 2)
                pos = length if end < 0 else end + 1
                continue
            if following == "*":
                end = text.find("*/", pos + 2)
                if end < 0:
                    raise ConfigParseError("unterminated block comment")
                pos = end + 2
                continue
        break
    return pos


def _skip_string(text: str, pos: int) -> int:
    quote = text[pos]
    pos += 1
    while pos < len(text):
        char = text[pos]
        if char == "\\":
            pos += 2
            continue
        if char == quote:
            return pos + 1
        pos += 1
    raise ConfigParseError("unterminated string while locating config assignment")


def _find_config_object(text: str) -> int:
    """Return the offset of the ``{`` assigned to ``config`` without executing."""
    pos = 0
    length = len(text)
    while pos < length:
        pos = _skip_js(text, pos)
        if pos >= length:
            break
        char = text[pos]
        if char in {"'", '"', "`"}:
            pos = _skip_string(text, pos)
            continue
        if char.isalpha() or char in "_$":
            match = _IDENTIFIER_RE.match(text, pos)
            if not match:
                pos += 1
                continue
            word = match.group(0)
            pos = match.end()
            if word != "config":
                continue
            after = _skip_js(text, pos)
            if after >= length or text[after] != "=":
                continue
            if text.startswith("==", after) or text.startswith("=>", after):
                continue
            after = _skip_js(text, after + 1)
            if after < length and text[after] == "{":
                return after
            continue
        pos += 1
    raise ConfigParseError("no `config = { ... }` object assignment was found")


def parse_config_js(text: str) -> dict[str, Any]:
    """Parse the object assigned to ``config`` without evaluating JavaScript.

    Raises :class:`ConfigParseError` when the assignment is missing or uses an
    expression the bounded reader cannot represent.
    """
    if not isinstance(text, str):  # pragma: no cover - defensive
        raise ConfigParseError("config source must be text")
    if len(text.encode("utf-8", "replace")) > MAX_SOURCE_BYTES:
        raise ConfigParseError("config source exceeds the parser size limit")
    start = _find_config_object(text)
    parser = _Parser(text, start)
    value = parser.parse_value()
    if not isinstance(value, dict):
        raise ConfigParseError("`config` must be an object")
    return value


def _slug(value: str) -> str:
    slug = re.sub(r"[^a-z0-9]+", "-", value.lower()).strip("-")
    return slug or "module"


def _reference_kind(path: str) -> str:
    lowered = path.lower().split("?")[0]
    if lowered.endswith(".css"):
        return "css"
    if lowered.endswith(_IMAGE_SUFFIXES):
        return "image"
    if lowered.endswith(_FONT_SUFFIXES):
        return "font"
    if lowered.endswith(_MEDIA_SUFFIXES):
        return "media"
    return "asset"


def _collect_references(value: Any, path: tuple[str, ...] = ()) -> list[dict[str, Any]]:
    """Collect CSS/asset strings without fetching or inlining them."""
    references: list[dict[str, Any]] = []
    if isinstance(value, Mapping):
        for key, item in value.items():
            key_text = str(key)
            key_lower = key_text.lower()
            child_path = (*path, key_text)
            if isinstance(item, str) and any(
                part in key_lower for part in _ASSET_KEY_PARTS
            ):
                lowered = item.lower()
                references.append(
                    {
                        "path": "/".join(child_path),
                        "value": item,
                        "kind": _reference_kind(item),
                        "remote": lowered.startswith(("http://", "https://", "//")),
                        "retained": True,
                        "inline": False,
                        "executable": False,
                        "sandbox": "none",
                    }
                )
            else:
                references.extend(_collect_references(item, child_path))
    elif isinstance(value, list):
        for index, item in enumerate(value):
            references.extend(
                _collect_references(item, (*path, str(index)))
            )
    return references


def _module_label(name: str, module: Mapping[str, Any]) -> str:
    header = module.get("header")
    if isinstance(header, str) and header.strip():
        return header
    return name


def _modules(config: Mapping[str, Any]) -> list[Any]:
    modules = config.get("modules")
    return list(modules) if isinstance(modules, list) else []


class MagicMirrorImporter(BaseImporter):
    """Import a MagicMirror ``config.js`` without executing it."""

    format = FORMAT
    version = VERSION
    suffixes = (".js",)
    mime = "text/javascript"

    def sniff(self, data: bytes) -> bool:
        lowered = data.lower()
        return b"config" in lowered and b"modules" in lowered

    def _asset(self, name: str, raw: bytes) -> ImportAsset:
        return ImportAsset(
            name or "config.js",
            raw,
            mime="text/javascript",
            kind="script",
            executable=True,
            sandbox="iframe",
        )

    def parse(
        self,
        name: str,
        data: bytes,
        context: ImportContext,
    ) -> ImportResult:
        text = data.decode("utf-8", "replace")
        asset = self._asset(name, data)
        warnings: list[str] = []
        unknown: dict[str, Any] = {}
        extensions: dict[str, Any] = {}
        widgets: list[dict[str, Any]] = []

        try:
            config = parse_config_js(text)
        except ConfigParseError as exc:
            warnings.append(
                "MagicMirror config could not be parsed without executing "
                f"JavaScript ({exc}); the complete source was retained opaque "
                "and no widgets were projected"
            )
            unknown = {
                "magicmirror": {
                    "opaque_source": text,
                    "parse_error": str(exc),
                    "executed": False,
                }
            }
            extensions = {
                "magicmirror": {
                    "opaque_source": text,
                    "parse_error": str(exc),
                    "executed": False,
                    "limits": {
                        "max_source_bytes": MAX_SOURCE_BYTES,
                        "max_depth": MAX_DEPTH,
                        "max_nodes": MAX_NODES,
                    },
                }
            }
            source_document = make_source_document(
                FORMAT,
                VERSION,
                data,
                original=asset,
                parsed=None,
                unknown=unknown,
            )
            source_capture = make_source_capture(
                FORMAT,
                VERSION,
                data,
                assets=[asset],
                captured=True,
                opaque=unknown,
                warnings=warnings,
            )
            return ImportResult(
                format=FORMAT,
                version=VERSION,
                raw=data,
                widgets=widgets,
                source_document=source_document,
                source_capture=source_capture,
                assets=[asset],
                unknown=unknown,
                warnings=warnings,
                extensions=extensions,
            )

        references = _collect_references(config)
        opaque_modules: list[dict[str, Any]] = []
        modules = _modules(config)
        if config.get("modules") is not None and not isinstance(
            config.get("modules"), list
        ):
            warnings.append("`modules` is not an array; retained opaque")

        for index, module in enumerate(modules):
            if not isinstance(module, Mapping):
                opaque_modules.append(
                    {"index": index, "module": None, "reason": "not an object",
                     "raw": module}
                )
                warnings.append(
                    f"modules[{index}] is not an object; retained opaque"
                )
                continue
            module_name = module.get("module")
            if not isinstance(module_name, str) or not module_name.strip():
                opaque_modules.append(
                    {"index": index, "module": None, "reason": "missing module name",
                     "raw": dict(module)}
                )
                warnings.append(
                    f"modules[{index}] has no module name; retained opaque"
                )
                continue
            module_name = module_name.strip()
            kind = _MODULE_KINDS.get(module_name.lower())
            if kind is None and module_name.startswith("MMM-"):
                kind = "card"
                warnings.append(
                    f"community module `{module_name}` mapped best-effort to card IR"
                )
            if kind is None:
                opaque_modules.append(
                    {
                        "index": index,
                        "module": module_name,
                        "reason": "unknown module",
                        "raw": dict(module),
                    }
                )
                warnings.append(
                    f"unknown module `{module_name}` retained opaque without a widget"
                )
                continue

            widget_id = f"magicmirror-{index}-{_slug(module_name)}"
            module_extensions = {
                "magicmirror": {
                    "module": module_name,
                    "index": index,
                    "position": module.get("position"),
                    "header": module.get("header"),
                    "config": dict(module),
                    "module_config": module.get("config"),
                    "raw_module": dict(module),
                    "references": references,
                    "executed": False,
                }
            }
            widgets.append(
                make_widget(
                    widget_id,
                    kind,
                    FORMAT,
                    data,
                    metric=module_name,
                    role=module_name,
                    label=_module_label(module_name, module),
                    extensions=module_extensions,
                    source_version=VERSION,
                    source_unknown=dict(module),
                )
            )

        unknown = {
            "magicmirror": {
                "module_count": len(modules),
                "unknown_modules": opaque_modules,
                "references": references,
                "asset_references": references,
                "executed": False,
            }
        }
        extensions = {
            "magicmirror": {
                "document": config,
                "modules": modules,
                "references": references,
                "asset_references": references,
                "unknown_modules": opaque_modules,
                "executed": False,
                "limits": {
                    "max_source_bytes": MAX_SOURCE_BYTES,
                    "max_depth": MAX_DEPTH,
                    "max_nodes": MAX_NODES,
                },
                "context": {
                    "name": context.name,
                    "base_dir": context.base_dir,
                },
            }
        }
        source_document = make_source_document(
            FORMAT,
            VERSION,
            data,
            original=asset,
            parsed=config,
            unknown=unknown,
        )
        source_capture = make_source_capture(
            FORMAT,
            VERSION,
            data,
            assets=[asset],
            captured=True,
            opaque=unknown,
            warnings=warnings,
        )
        return ImportResult(
            format=FORMAT,
            version=VERSION,
            raw=data,
            widgets=widgets,
            source_document=source_document,
            source_capture=source_capture,
            assets=[asset],
            unknown=unknown,
            warnings=warnings,
            extensions=extensions,
        )


#: Convenience alias for callers that name the config importer explicitly.
MagicMirrorConfigImporter = MagicMirrorImporter
