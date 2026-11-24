# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Controller-side protobuf Display protocol.

This module is the Python counterpart of the Rust ``ozma-display-protocol``
crate.  It implements the reliable Display channel framing:

    +---------+----------------------------+
    | tag (1) | protobuf payload (<=8 MiB) |
    +---------+----------------------------+

The protobuf payloads are produced by ``protoc`` from the shared schema in
``proto/ozma/v1`` (see ``scripts/gen_proto_python.sh``).  JSON remains the
canonical format for the display *definition document* and is still available
for legacy native-render clients through :class:`screen_manager.NativeRenderDriver`.

Tag values are part of the wire contract and must match the Rust codec in
``ozma-display-protocol/src/lib.rs``.
"""

from __future__ import annotations

import json
import math
import re
import sys
from pathlib import Path
from typing import Any, Iterable, Iterator, Mapping

# ---------------------------------------------------------------------------
# Generated protobuf modules
# ---------------------------------------------------------------------------
#
# ``protoc --python_out=controller/proto_gen -I proto`` writes modules whose
# imports look like ``from ozma.v1 import action_pb2``.  Put the generated
# directory first on ``sys.path`` so those imports resolve to our checked-in
# bindings and never to a system-wide ``ozma`` package.
_PROTO_GEN_DIR = Path(__file__).resolve().parent / "proto_gen"
_PROTO_GEN_STR = str(_PROTO_GEN_DIR)
if _PROTO_GEN_STR not in sys.path:
    sys.path.insert(0, _PROTO_GEN_STR)

try:
    from google.protobuf.message import DecodeError as _ProtobufDecodeError
    from ozma.v1 import action_pb2, data_pb2, scene_pb2
except ImportError as _exc:  # pragma: no cover - exercised only on broken installs
    raise ImportError(
        "Controller protobuf bindings are missing. Run "
        "scripts/gen_proto_python.sh from the repository root."
    ) from _exc


# ---------------------------------------------------------------------------
# Wire tags (must match ozma-display-protocol/src/lib.rs)
# ---------------------------------------------------------------------------

TAG_DEFINITION = 0x01
TAG_DEFINITION_REF = 0x02
TAG_DEFINITION_PATCH = 0x03
TAG_SCENE_COMMIT = 0x04
TAG_DATA_UPDATE = 0x05
TAG_ACTION_INVOKE = 0x06
TAG_INTERACTION_EVENT = 0x07
TAG_SCENE_SNAPSHOT = 0x08

HEADER_LEN = 1
MAX_PAYLOAD_LEN = 8 * 1024 * 1024
MAX_FRAME_LEN = HEADER_LEN + MAX_PAYLOAD_LEN

#: Tag -> (stable kind name, generated protobuf class).
MESSAGE_TYPES: dict[int, tuple[str, type[Any]]] = {
    TAG_DEFINITION: ("definition", data_pb2.DisplayDefinition),
    TAG_DEFINITION_REF: ("definition_ref", data_pb2.DefinitionRef),
    TAG_DEFINITION_PATCH: ("definition_patch", data_pb2.DefinitionPatch),
    TAG_SCENE_COMMIT: ("scene_commit", scene_pb2.SceneCommit),
    TAG_DATA_UPDATE: ("data_update", data_pb2.DataUpdate),
    TAG_ACTION_INVOKE: ("action_invoke", action_pb2.ActionInvoke),
    TAG_INTERACTION_EVENT: ("interaction_event", action_pb2.InteractionEvent),
    TAG_SCENE_SNAPSHOT: ("scene_snapshot", scene_pb2.SceneSnapshot),
}

TAG_BY_NAME: dict[str, int] = {name: tag for tag, (name, _) in MESSAGE_TYPES.items()}


class DisplayProtocolError(ValueError):
    """Base class for Display framing and definition errors."""


class EmptyFrameError(DisplayProtocolError):
    """A frame contained no tag byte."""


class OversizedFrameError(DisplayProtocolError):
    """A protobuf payload exceeded :data:`MAX_PAYLOAD_LEN`."""


class UnknownTagError(DisplayProtocolError):
    """A frame used a tag that does not map to a known message."""


class DisplayDecodeError(DisplayProtocolError):
    """A protobuf payload could not be parsed."""


class DisplayEncodeError(DisplayProtocolError):
    """A message could not be encoded."""


# ---------------------------------------------------------------------------
# Frame codec
# ---------------------------------------------------------------------------


def tag_for(kind: int | str) -> int:
    """Resolve a numeric tag or a stable kind name (``"data_update"``)."""
    if isinstance(kind, bool):
        raise TypeError("display tag must be an int or kind name, not bool")
    if isinstance(kind, int):
        if kind not in MESSAGE_TYPES:
            raise UnknownTagError(f"unknown display message tag 0x{kind:02x}")
        return kind
    if isinstance(kind, str):
        # Accept snake_case and proto-style spellings such as "DataUpdate".
        name = re.sub(r"(?<!^)(?=[A-Z])", "_", kind.strip()).lower()
        if name not in TAG_BY_NAME:
            raise UnknownTagError(f"unknown display message kind {kind!r}")
        return TAG_BY_NAME[name]
    raise TypeError(f"display tag must be int or str, got {type(kind).__name__}")


def kind_for_tag(tag: int) -> str:
    """Return the stable kind name for a wire tag."""
    try:
        return MESSAGE_TYPES[tag][0]
    except KeyError as exc:
        raise UnknownTagError(f"unknown display message tag 0x{tag:02x}") from exc


def encode_frame(tag: int | str, message: Any) -> bytes:
    """Encode ``message`` as ``[tag][protobuf payload]``.

    The reliable transport already carries the payload length, so no extra
    length prefix is added here.  ``message`` must be an instance of the
    generated class associated with ``tag``.
    """
    resolved = tag_for(tag)
    expected = MESSAGE_TYPES[resolved][1]
    if not isinstance(message, expected):
        raise TypeError(
            f"tag {kind_for_tag(resolved)!r} expects {expected.__name__}, "
            f"got {type(message).__name__}"
        )

    try:
        payload = message.SerializeToString(deterministic=True)
    except Exception as exc:  # pragma: no cover - defensive
        raise DisplayEncodeError(
            f"failed to encode {kind_for_tag(resolved)} payload"
        ) from exc

    if len(payload) > MAX_PAYLOAD_LEN:
        raise OversizedFrameError(
            f"display payload is {len(payload)} bytes, exceeding the "
            f"{MAX_PAYLOAD_LEN} byte limit"
        )
    return bytes((resolved,)) + payload


def decode_frame(frame: bytes | bytearray | memoryview) -> tuple[int, Any]:
    """Decode ``[tag][protobuf payload]`` into ``(tag, message)``.

    Empty frames, unknown tags, oversized payloads, and malformed protobuf
    payloads are rejected.  Unknown *fields* inside a known payload are
    skipped by normal proto3 semantics.
    """
    if not isinstance(frame, (bytes, bytearray, memoryview)):
        raise TypeError(f"display frame must be bytes-like, got {type(frame).__name__}")
    data = bytes(frame)
    if not data:
        raise EmptyFrameError("empty display frame: expected a 1-byte message tag")

    tag = data[0]
    payload = data[HEADER_LEN:]
    if len(payload) > MAX_PAYLOAD_LEN:
        raise OversizedFrameError(
            f"display payload is {len(payload)} bytes, exceeding the "
            f"{MAX_PAYLOAD_LEN} byte limit"
        )

    entry = MESSAGE_TYPES.get(tag)
    if entry is None:
        raise UnknownTagError(f"unknown display message tag 0x{tag:02x}")

    message = entry[1]()
    try:
        message.ParseFromString(payload)
    except (_ProtobufDecodeError, ValueError) as exc:
        raise DisplayDecodeError(
            f"failed to decode {entry[0]} payload: {exc}"
        ) from exc
    return tag, message


def decode_expected(
    frame: bytes | bytearray | memoryview,
    tag: int | str,
    message_type: type[Any],
) -> Any:
    """Decode a frame and assert it carries ``message_type``."""
    resolved = tag_for(tag)
    decoded_tag, message = decode_frame(frame)
    if decoded_tag != resolved:
        raise DisplayDecodeError(
            f"expected {kind_for_tag(resolved)} frame, "
            f"got {kind_for_tag(decoded_tag)}"
        )
    if not isinstance(message, message_type):
        raise DisplayDecodeError(
            f"expected {message_type.__name__}, got {type(message).__name__}"
        )
    return message


def _decode_payload_or_frame(
    data: bytes | bytearray | memoryview,
    tag: int,
    message_type: type[Any],
) -> Any:
    """Accept either a complete frame or a bare protobuf payload."""
    if not isinstance(data, (bytes, bytearray, memoryview)):
        raise TypeError(f"expected bytes-like, got {type(data).__name__}")
    raw = bytes(data)
    if raw and raw[0] == tag:
        return decode_expected(raw, tag, message_type)
    if not raw:
        raise EmptyFrameError("empty display payload: expected a frame or message body")
    if len(raw) > MAX_PAYLOAD_LEN:
        raise OversizedFrameError(
            f"display payload is {len(raw)} bytes, exceeding the "
            f"{MAX_PAYLOAD_LEN} byte limit"
        )
    message = message_type()
    try:
        message.ParseFromString(raw)
    except (_ProtobufDecodeError, ValueError) as exc:
        raise DisplayDecodeError(
            f"failed to decode {kind_for_tag(tag)} payload: {exc}"
        ) from exc
    return message


# ---------------------------------------------------------------------------
# Canonical JSON and definition hashing
# ---------------------------------------------------------------------------

_SAFE_INTEGER = 9_007_199_254_740_992  # 2**53


def _canonical_string(text: str) -> str:
    # json.dumps handles escaping deterministically.  ensure_ascii=False keeps
    # UTF-8 text as UTF-8, matching the Rust canonical encoder.
    return json.dumps(text, ensure_ascii=False, separators=(",", ":"))


def _canonical_number(value: int | float) -> str:
    if isinstance(value, int):  # bool is handled by the caller
        return str(value)
    if not math.isfinite(value):
        raise DisplayProtocolError("canonical JSON cannot encode NaN or infinity")
    if value == 0.0:  # also normalizes -0.0
        return "0"
    if value.is_integer() and abs(value) <= _SAFE_INTEGER:
        return str(int(value))
    text = repr(value)
    if "e" in text or "E" in text:
        mantissa, exponent = re.split("[eE]", text, maxsplit=1)
        sign = ""
        if exponent[:1] in {"+", "-"}:
            sign, exponent = exponent[0], exponent[1:]
        exponent = exponent.lstrip("0") or "0"
        # ryu/serde_json emits ``1e20`` and ``1e-7`` rather than ``1e+20``.
        text = f"{mantissa}e{sign if sign == '-' else ''}{exponent}"
    if text.endswith(".0"):
        text = text[:-2]
    return text


def _write_canonical(value: Any, out: list[str]) -> None:
    if value is None:
        out.append("null")
    elif value is True:
        out.append("true")
    elif value is False:
        out.append("false")
    elif isinstance(value, int):
        out.append(str(value))
    elif isinstance(value, float):
        out.append(_canonical_number(value))
    elif isinstance(value, str):
        out.append(_canonical_string(value))
    elif isinstance(value, (list, tuple)):
        out.append("[")
        for index, item in enumerate(value):
            if index:
                out.append(",")
            _write_canonical(item, out)
        out.append("]")
    elif isinstance(value, Mapping):
        out.append("{")
        keys = list(value.keys())
        if any(not isinstance(key, str) for key in keys):
            raise DisplayProtocolError("canonical JSON object keys must be strings")
        for index, key in enumerate(sorted(keys)):
            if index:
                out.append(",")
            out.append(_canonical_string(key))
            out.append(":")
            _write_canonical(value[key], out)
        out.append("}")
    else:
        raise DisplayProtocolError(
            f"canonical JSON cannot encode {type(value).__name__}"
        )


def canonical_json(value: Any) -> str:
    """Return deterministic, compact JSON with recursively sorted keys.

    Object keys are sorted by Unicode code point, arrays keep their order, and
    integers/floats use a stable spelling (``1.0`` becomes ``1`` and ``-0.0``
    becomes ``0``) so equivalent documents hash identically.
    """
    out: list[str] = []
    _write_canonical(value, out)
    return "".join(out)


def canonical_json_bytes(value: Any) -> bytes:
    """UTF-8 bytes of :func:`canonical_json`."""
    return canonical_json(value).encode("utf-8")


def definition_hash(document: Any) -> bytes:
    """Return the BLAKE3 hash of a canonical display-definition document.

    ``document`` may be a parsed JSON value, UTF-8 bytes/str of an already
    canonical document, or a generated ``DisplayDefinition`` (whose
    ``definition_json`` is hashed).
    """
    if isinstance(document, data_pb2.DisplayDefinition):
        document = bytes(document.definition_json)

    if isinstance(document, (bytes, bytearray, memoryview)):
        raw = bytes(document)
        try:
            parsed: Any = json.loads(raw.decode("utf-8"))
        except (UnicodeDecodeError, ValueError):
            payload = raw
        else:
            payload = canonical_json_bytes(parsed)
    elif isinstance(document, str):
        try:
            parsed = json.loads(document)
        except ValueError:
            payload = document.encode("utf-8")
        else:
            payload = canonical_json_bytes(parsed)
    else:
        payload = canonical_json_bytes(document)
    try:
        import blake3
    except ImportError as exc:  # pragma: no cover - requirements install it
        raise DisplayProtocolError(
            "blake3 is required for definition hashing; install blake3"
        ) from exc
    return blake3.blake3(payload).digest()


# ---------------------------------------------------------------------------
# Definition field discovery and stable field IDs
# ---------------------------------------------------------------------------

#: Layout keys whose string value is a metric reference.
_METRIC_KEYS = {
    "metric",
    "metric_key",
    "metric_ref",
    "metric_id",
    "binding",
    "data_key",
    "data_key_ref",
}

#: Layout keys whose string value names an action.
_ACTION_KEYS = {
    "action",
    "tap_action",
    "press_action",
    "on_press",
    "on_tap",
    "on_click",
    "activate_action",
}

_VAR_RE = re.compile(r"\{([A-Za-z_][A-Za-z0-9_.:/-]*)\}")

#: Variables supplied by the screen manager rather than a metric collector.
_STRING_VARIABLES = {
    "scenario_name",
    "scenario_color",
    "scenario_id",
    "node_name",
    "time",
    "date",
}

#: Lower-case document type names, matching the Rust definition document.
TYPE_NAMES: dict[int, str] = {
    data_pb2.VALUE_TYPE_SINT: "sint",
    data_pb2.VALUE_TYPE_UINT: "uint",
    data_pb2.VALUE_TYPE_REAL: "real",
    data_pb2.VALUE_TYPE_BOOL: "bool",
    data_pb2.VALUE_TYPE_STRING: "string",
    data_pb2.VALUE_TYPE_ENUM: "enum",
    data_pb2.VALUE_TYPE_BYTES: "bytes",
    data_pb2.VALUE_TYPE_TIMESTAMP: "timestamp",
}

TYPE_BY_NAME: dict[str, int] = {
    "sint": data_pb2.VALUE_TYPE_SINT,
    "int": data_pb2.VALUE_TYPE_SINT,
    "integer": data_pb2.VALUE_TYPE_SINT,
    "uint": data_pb2.VALUE_TYPE_UINT,
    "unsigned": data_pb2.VALUE_TYPE_UINT,
    "real": data_pb2.VALUE_TYPE_REAL,
    "float": data_pb2.VALUE_TYPE_REAL,
    "double": data_pb2.VALUE_TYPE_REAL,
    "number": data_pb2.VALUE_TYPE_REAL,
    "bool": data_pb2.VALUE_TYPE_BOOL,
    "boolean": data_pb2.VALUE_TYPE_BOOL,
    "string": data_pb2.VALUE_TYPE_STRING,
    "str": data_pb2.VALUE_TYPE_STRING,
    "text": data_pb2.VALUE_TYPE_STRING,
    "enum": data_pb2.VALUE_TYPE_ENUM,
    "bytes": data_pb2.VALUE_TYPE_BYTES,
    "blob": data_pb2.VALUE_TYPE_BYTES,
    "timestamp": data_pb2.VALUE_TYPE_TIMESTAMP,
    "timestamp_micros": data_pb2.VALUE_TYPE_TIMESTAMP,
}


def value_type_for_name(name: Any, *, default: int = data_pb2.VALUE_TYPE_REAL) -> int:
    """Normalize a document type string to a ``ValueType`` enum value."""
    if name is None or name == "":
        return default
    if isinstance(name, bool):
        raise DisplayProtocolError(f"invalid field value type {name!r}")
    if isinstance(name, int):
        if name in TYPE_NAMES:
            return name
        raise DisplayProtocolError(f"unknown field value type {name!r}")
    key = str(name).strip().lower()
    if key.startswith("value_type_"):
        key = key[len("value_type_"):]
    if key == "unspecified":
        return default
    try:
        return TYPE_BY_NAME[key]
    except KeyError as exc:
        raise DisplayProtocolError(f"unknown field value type {name!r}") from exc


class IdAllocator:
    """Explicit, monotonic identifier accumulator.

    IDs start at ``1`` and are never reused.  An explicit ID may be reserved
    first (for definitions authored with a pre-existing field table); the
    accumulator skips any reserved values.
    """

    __slots__ = ("_next", "_used")

    def __init__(self, start: int = 1) -> None:
        if start < 1:
            raise ValueError("identifier allocator must start at 1 or greater")
        self._next = start
        self._used: set[int] = set()

    @property
    def next_id(self) -> int:
        """The next identifier that :meth:`allocate` will return."""
        value = self._next
        while value in self._used:
            value += 1
        return value

    def reserve(self, value: int) -> int:
        """Reserve an explicit identifier, rejecting duplicates."""
        if not isinstance(value, int) or isinstance(value, bool) or value < 1:
            raise DisplayProtocolError(f"invalid field/action id {value!r}")
        if value in self._used:
            raise DisplayProtocolError(f"duplicate field/action id {value}")
        self._used.add(value)
        if value >= self._next:
            self._next = value + 1
        return value

    def allocate(self) -> int:
        """Allocate the next unused identifier."""
        value = self.next_id
        self._used.add(value)
        self._next = value + 1
        return value


#: Backwards-compatible alias with a more descriptive name.
FieldIdAllocator = IdAllocator


def _infer_field_type(key: str) -> int:
    """Infer a reasonable type for a metric or variable key.

    Explicit layout metadata always wins; this is only a deterministic default
    so the compiled field table is usable without per-widget annotations.
    """
    base = key.lower()
    leaf = re.split(r"[./:]", base)[-1]
    if base in _STRING_VARIABLES or leaf in _STRING_VARIABLES:
        return data_pb2.VALUE_TYPE_STRING
    if base.startswith(("scenario_", "node_")):
        return data_pb2.VALUE_TYPE_STRING
    if any(token in leaf for token in ("name", "color", "colour", "label", "text", "status", "state", "mode")):
        return data_pb2.VALUE_TYPE_STRING
    if any(token in leaf for token in ("pct", "percent", "temp", "usage", "rate", "level", "fps", "draw", "power", "volt", "gain", "db", "load")):
        return data_pb2.VALUE_TYPE_REAL
    if any(token in leaf for token in ("count", "bytes", "rpm", "mhz", "hz", "uptime", "seconds")):
        return data_pb2.VALUE_TYPE_UINT
    if leaf.endswith(("_ms", "_us")):
        return data_pb2.VALUE_TYPE_UINT
    if leaf in {"online", "enabled", "active", "muted"}:
        return data_pb2.VALUE_TYPE_BOOL
    return data_pb2.VALUE_TYPE_REAL


def _spec_from_explicit(value: Any) -> dict[str, Any]:
    """Normalize a layout ``fields`` entry into a spec dict."""
    if value is None:
        return {}
    if isinstance(value, str):
        return {"type": value_type_for_name(value)}
    if isinstance(value, int) and not isinstance(value, bool):
        return {"type": value_type_for_name(value)}
    if isinstance(value, Mapping):
        spec: dict[str, Any] = {}
        type_value = value.get("type", value.get("value_type"))
        if type_value is not None:
            spec["type"] = value_type_for_name(type_value)
        if value.get("unit") is not None:
            spec["unit"] = str(value["unit"])
        if value.get("scale") is not None:
            spec["scale"] = int(value["scale"])
        if value.get("description") is not None:
            spec["description"] = str(value["description"])
        if value.get("id") is not None:
            spec["id"] = int(value["id"])
        return spec
    raise DisplayProtocolError(f"invalid field metadata {value!r}")


def _iter_layout_strings(value: Any) -> Iterator[str]:
    if isinstance(value, str):
        yield value
    elif isinstance(value, Mapping):
        for item in value.values():
            yield from _iter_layout_strings(item)
    elif isinstance(value, (list, tuple)):
        for item in value:
            yield from _iter_layout_strings(item)


def collect_field_specs(layout: Mapping[str, Any] | None) -> dict[str, dict[str, Any]]:
    """Collect fields referenced by a layout document.

    References come from ``metric``-style widget attributes, ``{variable}``
    placeholders in label/text strings, and an optional explicit ``fields`` or
    ``field_types`` table.  Explicit metadata overrides inferred types.
    """
    specs: dict[str, dict[str, Any]] = {}
    if not isinstance(layout, Mapping):
        return specs

    explicit = layout.get("fields")
    explicit_map: Mapping[str, Any] = {}
    if isinstance(explicit, Mapping):
        explicit_map = explicit
        for key, metadata in explicit.items():
            if not isinstance(key, str) or not key:
                continue
            specs[key] = _spec_from_explicit(metadata)
    elif isinstance(explicit, list):
        explicit_map = {str(item.get("key")): item for item in explicit if isinstance(item, Mapping) and item.get("key")}
        for key, metadata in explicit_map.items():
            specs[key] = _spec_from_explicit(metadata)

    field_types = layout.get("field_types")
    if isinstance(field_types, Mapping):
        for key, metadata in field_types.items():
            if isinstance(key, str) and key:
                specs.setdefault(key, _spec_from_explicit(metadata))

    def _add(key: Any, metadata: Any = None) -> None:
        if not isinstance(key, str):
            return
        key = key.strip()
        if not key or key == "*":
            return
        spec = specs.setdefault(key, {})
        if metadata is not None:
            explicit_spec = _spec_from_explicit(metadata)
            spec.update(explicit_spec)

    def _walk(node: Any) -> None:
        if isinstance(node, Mapping):
            for key, value in node.items():
                if key in {"fields", "field_types", "actions"}:
                    # Compiled tables are read separately; do not treat their
                    # metadata strings as new metric references.
                    continue
                if key in _METRIC_KEYS and isinstance(value, str):
                    for metric in re.split(r"[\s,]+", value):
                        _add(metric)
                elif key in {"metrics", "metric_keys"}:
                    if isinstance(value, str):
                        _add(value)
                    elif isinstance(value, (list, tuple)):
                        for metric in value:
                            _add(metric)
                if isinstance(value, str):
                    for placeholder in _VAR_RE.findall(value):
                        _add(placeholder)
                elif isinstance(value, (Mapping, list, tuple)):
                    _walk(value)
        elif isinstance(node, (list, tuple)):
            for item in node:
                _walk(item)

    _walk(layout)

    # Explicit table entries win over inferred references.
    for key, metadata in explicit_map.items():
        if key:
            specs.setdefault(str(key), {}).update(_spec_from_explicit(metadata))
    for key in list(specs):
        specs[key].setdefault("type", _infer_field_type(key))
    return specs


def _collect_action_specs(layout: Mapping[str, Any] | None) -> dict[str, dict[str, Any]]:
    """Collect action descriptors referenced by a layout document."""
    actions: dict[str, dict[str, Any]] = {}
    if not isinstance(layout, Mapping):
        return actions

    def _add(key: Any, metadata: Any = None) -> None:
        if isinstance(key, str):
            key = key.strip()
        else:
            return
        if not key:
            return
        spec = actions.setdefault(key, {})
        if isinstance(metadata, Mapping):
            params = metadata.get("params_schema", metadata.get("params_schema_json"))
            if params is not None:
                spec["params_schema"] = params
            if metadata.get("permissions") is not None:
                spec["permissions"] = list(metadata["permissions"])
            if metadata.get("requires_confirmation") is not None:
                spec["requires_confirmation"] = bool(metadata["requires_confirmation"])
            if metadata.get("label") is not None:
                spec["label"] = str(metadata["label"])
            if metadata.get("description") is not None:
                spec["description"] = str(metadata["description"])
            if metadata.get("id") is not None:
                spec["id"] = int(metadata["id"])

    explicit = layout.get("actions")
    if isinstance(explicit, Mapping):
        for key, metadata in explicit.items():
            _add(key, metadata)
    elif isinstance(explicit, list):
        for item in explicit:
            if isinstance(item, str):
                _add(item)
            elif isinstance(item, Mapping) and item.get("key"):
                _add(item["key"], item)

    def _walk(node: Any) -> None:
        if isinstance(node, Mapping):
            for key, value in node.items():
                if key in _ACTION_KEYS and isinstance(value, str):
                    _add(value)
                elif isinstance(value, (Mapping, list, tuple)):
                    _walk(value)
        elif isinstance(node, (list, tuple)):
            for item in node:
                _walk(item)

    _walk(layout)
    return actions


def _field_entries(specs: Mapping[str, Mapping[str, Any]]) -> tuple[list[dict[str, Any]], list[data_pb2.FieldDef]]:
    """Assign stable IDs and build document/proto field tables.

    Keys are sorted explicitly so ID assignment does not depend on Python dict
    insertion order.  The allocator is the source of numeric IDs.
    """
    allocator = IdAllocator()
    ordered_keys = sorted(specs)
    # Reserve explicitly-authored IDs first so they are never duplicated.
    assigned: dict[str, int] = {}
    for key in ordered_keys:
        explicit_id = specs[key].get("id")
        if explicit_id is not None:
            assigned[key] = allocator.reserve(int(explicit_id))
    # Allocate the remaining IDs in explicit sorted-key order.
    for key in ordered_keys:
        if key not in assigned:
            assigned[key] = allocator.allocate()

    document_entries: list[dict[str, Any]] = []
    proto_entries: list[data_pb2.FieldDef] = []
    for key in ordered_keys:
        spec = specs[key]
        field_id = assigned[key]
        field_type = value_type_for_name(spec.get("type"), default=_infer_field_type(key))
        entry: dict[str, Any] = {
            "id": field_id,
            "key": key,
            "type": TYPE_NAMES[field_type],
        }
        if spec.get("unit") is not None:
            entry["unit"] = str(spec["unit"])
        if spec.get("scale") is not None:
            entry["scale"] = int(spec["scale"])
        if spec.get("description") is not None:
            entry["description"] = str(spec["description"])
        document_entries.append(entry)

        proto_entry = data_pb2.FieldDef(id=field_id, key=key, type=field_type)
        if spec.get("unit") is not None:
            proto_entry.unit = str(spec["unit"])
        if spec.get("scale") is not None:
            proto_entry.scale = int(spec["scale"])
        if spec.get("description") is not None:
            proto_entry.description = str(spec["description"])
        proto_entries.append(proto_entry)

    return document_entries, proto_entries


def _action_entries(specs: Mapping[str, Mapping[str, Any]]) -> tuple[list[dict[str, Any]], list[action_pb2.ActionDef]]:
    """Assign stable action IDs and build document/proto action tables."""
    allocator = IdAllocator()
    ordered_keys = sorted(specs)
    assigned: dict[str, int] = {}
    for key in ordered_keys:
        explicit_id = specs[key].get("id")
        if explicit_id is not None:
            assigned[key] = allocator.reserve(int(explicit_id))
    for key in ordered_keys:
        if key not in assigned:
            assigned[key] = allocator.allocate()

    document_entries: list[dict[str, Any]] = []
    proto_entries: list[action_pb2.ActionDef] = []
    for key in ordered_keys:
        spec = specs[key]
        action_id = assigned[key]
        params_schema = spec.get("params_schema")
        if params_schema is None:
            params_schema_json = canonical_json_bytes({})
            params_schema_doc: Any = {}
        elif isinstance(params_schema, (bytes, bytearray)):
            params_schema_json = bytes(params_schema)
            try:
                params_schema_doc = json.loads(params_schema_json)
            except (ValueError, TypeError):
                params_schema_doc = {"type": "object"}
        elif isinstance(params_schema, str):
            params_schema_json = params_schema.encode("utf-8")
            try:
                params_schema_doc = json.loads(params_schema)
            except ValueError:
                params_schema_doc = {"type": "object"}
        else:
            params_schema_doc = params_schema
            params_schema_json = canonical_json_bytes(params_schema)

        entry: dict[str, Any] = {
            "id": action_id,
            "key": key,
            "params_schema": params_schema_doc,
        }
        if spec.get("permissions"):
            entry["permissions"] = [str(item) for item in spec["permissions"]]
        if spec.get("requires_confirmation"):
            entry["requires_confirmation"] = True
        if spec.get("label") is not None:
            entry["label"] = str(spec["label"])
        if spec.get("description") is not None:
            entry["description"] = str(spec["description"])
        document_entries.append(entry)

        proto_entry = action_pb2.ActionDef(
            id=action_id,
            key=key,
            params_schema_json=params_schema_json,
        )
        if spec.get("permissions"):
            proto_entry.permissions.extend(str(item) for item in spec["permissions"])
        if spec.get("requires_confirmation"):
            proto_entry.requires_confirmation = True
        if spec.get("label") is not None:
            proto_entry.label = str(spec["label"])
        if spec.get("description") is not None:
            proto_entry.description = str(spec["description"])
        proto_entries.append(proto_entry)

    return document_entries, proto_entries


def build_definition(
    layout: Mapping[str, Any] | None,
    screen_id: str,
    version: int,
) -> data_pb2.DisplayDefinition:
    """Build a ``DisplayDefinition`` from a screen layout document.

    The compiled field and action tables are derived from the layout's metric
    and action references.  Numeric IDs come from explicit monotonic
    allocators and are assigned in an explicitly sorted key order, so the same
    logical layout always yields the same table regardless of dict order.
    """
    if not isinstance(layout, Mapping):
        raise TypeError("layout must be a mapping")
    if not isinstance(screen_id, str) or not screen_id:
        raise ValueError("screen_id must be a non-empty string")
    version = int(version)
    if version < 0 or version > 0xFFFFFFFF:
        raise ValueError("version must fit in uint32")

    field_specs = collect_field_specs(layout)
    action_specs = _collect_action_specs(layout)
    field_doc, field_proto = _field_entries(field_specs)
    action_doc, action_proto = _action_entries(action_specs)

    document = {
        "id": screen_id,
        "version": version,
        "layout": layout,
        "fields": field_doc,
        "actions": action_doc,
    }
    definition_json = canonical_json_bytes(document)
    digest = definition_hash(definition_json)

    message = data_pb2.DisplayDefinition(
        id=screen_id,
        version=version,
        hash=digest,
        definition_json=definition_json,
    )
    message.fields.extend(field_proto)
    message.actions.extend(action_proto)
    return message


# ---------------------------------------------------------------------------
# Data updates
# ---------------------------------------------------------------------------


def _field_maps(definition: data_pb2.DisplayDefinition) -> tuple[dict[str, data_pb2.FieldDef], dict[int, data_pb2.FieldDef]]:
    by_key = {field.key: field for field in definition.fields}
    by_id = {field.id: field for field in definition.fields}
    return by_key, by_id


def _scaled_integer(field: data_pb2.FieldDef, value: Any) -> int | None:
    """Scale a numeric value for a ``sint``/``uint`` field.

    Integer inputs are already at wire scale and are preserved exactly, matching
    the Rust definition encoder.  Float inputs are multiplied by the declared
    scale and must land on an exactly representable integer.
    """
    if isinstance(value, bool):
        return int(value)
    if isinstance(value, int):
        return value
    if not isinstance(value, float) or not math.isfinite(value):
        return None
    scale = field.scale if field.HasField("scale") else 1
    scaled = value * scale
    if not math.isfinite(scaled):
        return None
    rounded = round(scaled)
    tolerance = 1e-9 * max(abs(scaled), 1.0)
    if abs(scaled - rounded) > tolerance or abs(rounded) > _SAFE_INTEGER:
        return None
    return int(rounded)


def _encode_field_value(field: data_pb2.FieldDef, value: Any) -> data_pb2.FieldValue:
    """Build a typed ``FieldValue`` for ``field``/``value``.

    The declared field type is authoritative when the runtime value is
    compatible; otherwise the value's own Python type is preserved.
    """
    result = data_pb2.FieldValue(field_id=field.id)
    field_type = field.type

    if field_type == data_pb2.VALUE_TYPE_STRING:
        result.text = value if isinstance(value, str) else str(value)
        return result
    if field_type == data_pb2.VALUE_TYPE_BOOL:
        result.flag = bool(value)
        return result
    if field_type == data_pb2.VALUE_TYPE_BYTES:
        if isinstance(value, str):
            result.blob = value.encode("utf-8")
        elif isinstance(value, (bytes, bytearray, memoryview)):
            result.blob = bytes(value)
        elif isinstance(value, (list, tuple)):
            result.blob = bytes(int(byte) for byte in value)
        else:
            result.blob = bytes(value)
        return result
    if field_type == data_pb2.VALUE_TYPE_TIMESTAMP:
        result.timestamp_micros = int(value)
        return result
    if field_type == data_pb2.VALUE_TYPE_ENUM:
        result.enum_value = int(value)
        return result
    if field_type == data_pb2.VALUE_TYPE_UINT:
        scaled = _scaled_integer(field, value)
        if scaled is not None:
            if scaled >= 0:
                result.unsigned = scaled
            else:
                result.integer = scaled
        elif isinstance(value, (int, float)) and not isinstance(value, bool):
            result.real = float(value)
        elif isinstance(value, str):
            result.text = value
        else:
            result.text = str(value)
        return result
    if field_type == data_pb2.VALUE_TYPE_SINT:
        scaled = _scaled_integer(field, value)
        if scaled is not None:
            result.integer = scaled
        elif isinstance(value, (int, float)) and not isinstance(value, bool):
            result.real = float(value)
        elif isinstance(value, str):
            result.text = value
        else:
            result.text = str(value)
        return result
    if field_type == data_pb2.VALUE_TYPE_REAL:
        if isinstance(value, bool):
            result.flag = value
        elif isinstance(value, (int, float)):
            result.real = float(value)
        elif isinstance(value, str):
            result.text = value
        else:
            result.text = str(value)
        return result

    # Unknown/unspecified type: preserve the runtime Python type.
    if isinstance(value, bool):
        result.flag = value
    elif isinstance(value, int):
        result.integer = value
    elif isinstance(value, float):
        result.real = value
    elif isinstance(value, str):
        result.text = value
    elif isinstance(value, (bytes, bytearray, memoryview)):
        result.blob = bytes(value)
    else:
        result.text = str(value)
    return result


def build_data_update(
    definition: data_pb2.DisplayDefinition,
    values: Mapping[str | int, Any],
    *,
    field_ids: Iterable[int | str] | None = None,
    sequence: int | None = None,
    timestamp_micros: int | None = None,
) -> data_pb2.DataUpdate:
    """Build a compact ``DataUpdate`` carrying only requested fields.

    ``values`` may be keyed by definition field key or numeric field ID.  When
    ``field_ids`` is supplied, only those fields are considered.  Values whose
    keys are absent from the definition are ignored, as are ``None`` values.
    """
    if not isinstance(definition, data_pb2.DisplayDefinition):
        raise TypeError("definition must be a DisplayDefinition")
    by_key, by_id = _field_maps(definition)

    requested: set[int] | None = None
    if field_ids is not None:
        requested = set()
        for item in field_ids:
            if isinstance(item, str):
                field = by_key.get(item)
                if field is None:
                    continue
                requested.add(field.id)
            else:
                requested.add(int(item))

    update = data_pb2.DataUpdate(definition_version=definition.version)
    if sequence is not None:
        update.sequence = int(sequence)
    if timestamp_micros is not None:
        update.timestamp_micros = int(timestamp_micros)

    chosen: dict[int, Any] = {}
    for key, value in values.items():
        if value is None:
            continue
        if isinstance(key, str):
            field = by_key.get(key)
        else:
            field = by_id.get(int(key))
        if field is None:
            continue
        if requested is not None and field.id not in requested:
            continue
        chosen[field.id] = (field, value)

    for field_id in sorted(chosen):
        field, value = chosen[field_id]
        update.values.append(_encode_field_value(field, value))
    return update


def encode_data_update(
    definition: data_pb2.DisplayDefinition,
    values: Mapping[str | int, Any],
    *,
    field_ids: Iterable[int | str] | None = None,
    sequence: int | None = None,
    timestamp_micros: int | None = None,
) -> bytes:
    """Encode a complete Display frame containing a compact ``DataUpdate``."""
    return encode_frame(
        TAG_DATA_UPDATE,
        build_data_update(
            definition,
            values,
            field_ids=field_ids,
            sequence=sequence,
            timestamp_micros=timestamp_micros,
        ),
    )


# ---------------------------------------------------------------------------
# Action and interaction helpers
# ---------------------------------------------------------------------------


def decode_action_invoke(
    data: bytes | bytearray | memoryview,
) -> action_pb2.ActionInvoke:
    """Decode a tagged ActionInvoke frame or a bare ActionInvoke payload."""
    return _decode_payload_or_frame(data, TAG_ACTION_INVOKE, action_pb2.ActionInvoke)


def decode_interaction_event(
    data: bytes | bytearray | memoryview,
) -> action_pb2.InteractionEvent:
    """Decode a tagged InteractionEvent frame or a bare event payload."""
    return _decode_payload_or_frame(
        data, TAG_INTERACTION_EVENT, action_pb2.InteractionEvent
    )


def action_invoke_params(invoke: action_pb2.ActionInvoke) -> Any:
    """Return an ActionInvoke's parameters as parsed JSON where possible."""
    if invoke.HasField("params_json"):
        raw = bytes(invoke.params_json)
        try:
            return json.loads(raw.decode("utf-8"))
        except (ValueError, UnicodeDecodeError):
            return raw
    if invoke.params:
        raw = bytes(invoke.params)
        try:
            return json.loads(raw.decode("utf-8"))
        except (ValueError, UnicodeDecodeError):
            return raw
    return {}


def action_invoke_to_dict(invoke: action_pb2.ActionInvoke) -> dict[str, Any]:
    """Convert an ``ActionInvoke`` into callback-friendly plain data."""
    return {
        "action_id": invoke.action_id,
        "idempotency_key": invoke.idempotency_key,
        "params": action_invoke_params(invoke),
        "timestamp_micros": invoke.timestamp_micros
        if invoke.HasField("timestamp_micros")
        else None,
    }


_POINTER_PHASE_NAMES = {
    action_pb2.POINTER_PHASE_UNSPECIFIED: "unspecified",
    action_pb2.POINTER_PHASE_DOWN: "down",
    action_pb2.POINTER_PHASE_MOVE: "move",
    action_pb2.POINTER_PHASE_UP: "up",
    action_pb2.POINTER_PHASE_CANCEL: "cancel",
    action_pb2.POINTER_PHASE_WHEEL: "wheel",
}

_KEY_ACTION_NAMES = {
    action_pb2.KEY_ACTION_UNSPECIFIED: "unspecified",
    action_pb2.KEY_ACTION_DOWN: "down",
    action_pb2.KEY_ACTION_UP: "up",
    action_pb2.KEY_ACTION_PRESS: "press",
}


def interaction_event_to_dict(event: action_pb2.InteractionEvent) -> dict[str, Any]:
    """Convert an ``InteractionEvent`` into callback-friendly plain data."""
    result: dict[str, Any] = {"node_id": event.node_id}
    if event.HasField("target_id"):
        result["target_id"] = event.target_id
    if event.HasField("timestamp_micros"):
        result["timestamp_micros"] = event.timestamp_micros

    kind = event.WhichOneof("event")
    result["event"] = kind
    if kind == "pointer":
        pointer = event.pointer
        payload: dict[str, Any] = {
            "x": pointer.x,
            "y": pointer.y,
            "phase": _POINTER_PHASE_NAMES.get(pointer.phase, str(pointer.phase)),
        }
        if pointer.HasField("button"):
            payload["button"] = pointer.button
        if pointer.HasField("pressure_ppt"):
            payload["pressure_ppt"] = pointer.pressure_ppt
        if pointer.HasField("wheel_delta"):
            payload["wheel_delta"] = pointer.wheel_delta
        result["pointer"] = payload
    elif kind == "key":
        key = event.key
        result["key"] = {
            "key": key.key,
            "action": _KEY_ACTION_NAMES.get(key.action, str(key.action)),
            "modifiers": list(key.modifiers),
            "repeat": key.repeat if key.HasField("repeat") else None,
        }
    elif kind == "semantic":
        result["semantic"] = {
            "name": event.semantic.name,
            "params": dict(event.semantic.params),
        }
    elif kind == "action":
        result["action"] = action_invoke_to_dict(event.action)
    return result


# ---------------------------------------------------------------------------
# Legacy JSON compatibility
# ---------------------------------------------------------------------------


def json_layout_message(layout: Mapping[str, Any]) -> str:
    """Legacy JSON layout frame used by :class:`NativeRenderDriver`."""
    return json.dumps({"type": "layout", "layout": layout})


def json_data_message(values: Mapping[str, Any]) -> str:
    """Legacy JSON data frame used by :class:`NativeRenderDriver`."""
    return json.dumps({"type": "data", "d": values})


__all__ = [
    "TAG_DEFINITION",
    "TAG_DEFINITION_REF",
    "TAG_DEFINITION_PATCH",
    "TAG_SCENE_COMMIT",
    "TAG_DATA_UPDATE",
    "TAG_ACTION_INVOKE",
    "TAG_INTERACTION_EVENT",
    "TAG_SCENE_SNAPSHOT",
    "HEADER_LEN",
    "MAX_PAYLOAD_LEN",
    "MAX_FRAME_LEN",
    "MESSAGE_TYPES",
    "TAG_BY_NAME",
    "TYPE_NAMES",
    "TYPE_BY_NAME",
    "DisplayProtocolError",
    "EmptyFrameError",
    "OversizedFrameError",
    "UnknownTagError",
    "DisplayDecodeError",
    "DisplayEncodeError",
    "tag_for",
    "kind_for_tag",
    "encode_frame",
    "decode_frame",
    "decode_expected",
    "canonical_json",
    "canonical_json_bytes",
    "definition_hash",
    "IdAllocator",
    "FieldIdAllocator",
    "value_type_for_name",
    "collect_field_specs",
    "build_definition",
    "build_data_update",
    "encode_data_update",
    "decode_action_invoke",
    "decode_interaction_event",
    "action_invoke_params",
    "action_invoke_to_dict",
    "interaction_event_to_dict",
    "json_layout_message",
    "json_data_message",
]
