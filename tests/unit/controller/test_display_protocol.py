"""Tests for the controller-side protobuf Display protocol."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

import display_protocol as dp
from ozma.v1 import action_pb2, data_pb2, scene_pb2

pytestmark = pytest.mark.unit


def _reverse_object_keys(value):
    """Recursively rebuild mappings in reverse key order (arrays untouched)."""
    if isinstance(value, dict):
        return {key: _reverse_object_keys(value[key]) for key in reversed(list(value))}
    if isinstance(value, list):
        return [_reverse_object_keys(item) for item in value]
    return value


def test_generated_bindings_import_and_all_tags_round_trip():
    messages = {
        dp.TAG_DEFINITION: data_pb2.DisplayDefinition(id="def"),
        dp.TAG_DEFINITION_REF: data_pb2.DefinitionRef(id="def", version=1),
        dp.TAG_DEFINITION_PATCH: data_pb2.DefinitionPatch(definition_id="def"),
        dp.TAG_SCENE_COMMIT: scene_pb2.SceneCommit(revision=2),
        dp.TAG_DATA_UPDATE: data_pb2.DataUpdate(definition_version=3),
        dp.TAG_ACTION_INVOKE: action_pb2.ActionInvoke(action_id=4),
        dp.TAG_INTERACTION_EVENT: action_pb2.InteractionEvent(node_id=5),
        dp.TAG_SCENE_SNAPSHOT: scene_pb2.SceneSnapshot(revision=6),
    }

    assert set(dp.MESSAGE_TYPES) == set(range(1, 9))
    for tag, message in messages.items():
        frame = dp.encode_frame(tag, message)
        decoded_tag, decoded = dp.decode_frame(frame)
        assert decoded_tag == tag
        assert type(decoded) is type(message)
        assert decoded == message

    # Stable tag values match the Rust codec.
    assert [dp.TAG_DEFINITION, dp.TAG_DEFINITION_REF, dp.TAG_DEFINITION_PATCH,
            dp.TAG_SCENE_COMMIT, dp.TAG_DATA_UPDATE, dp.TAG_ACTION_INVOKE,
            dp.TAG_INTERACTION_EVENT, dp.TAG_SCENE_SNAPSHOT] == list(range(1, 9))


def test_frame_codec_rejects_empty_unknown_oversized_and_malformed(monkeypatch):
    with pytest.raises(dp.EmptyFrameError):
        dp.decode_frame(b"")

    with pytest.raises(dp.UnknownTagError):
        dp.decode_frame(b"\x09")
    with pytest.raises(dp.UnknownTagError):
        dp.encode_frame(0x09, data_pb2.DataUpdate())

    with pytest.raises(TypeError):
        dp.encode_frame(dp.TAG_DATA_UPDATE, action_pb2.ActionInvoke())

    # A truncated protobuf varint must be rejected, not silently accepted.
    with pytest.raises(dp.DisplayDecodeError):
        dp.decode_frame(b"\x05\x08")

    monkeypatch.setattr(dp, "MAX_PAYLOAD_LEN", 4)
    with pytest.raises(dp.OversizedFrameError):
        dp.decode_frame(b"\x05" + b"\x00" * 5)
    with pytest.raises(dp.OversizedFrameError):
        dp.encode_frame(
            dp.TAG_DATA_UPDATE,
            data_pb2.DataUpdate(definition_version=1, values=[
                data_pb2.FieldValue(field_id=1, text="too long"),
            ]),
        )


def test_canonical_fixture_matches_the_rust_language_vector():
    fixture = Path(__file__).resolve().parents[2] / "fixtures" / "display_definition.json"
    raw = fixture.read_bytes()
    document = json.loads(raw)

    canonical = dp.canonical_json_bytes(document)
    assert b'"numbers":[1.5,65.2,1e20,1e-7,0]' in canonical
    assert "Grüße".encode() in canonical
    assert dp.definition_hash(document).hex() == (
        "6afa1c4d445e67f71df2f978e029ad2c8399687d443bb7ce4a8436d5d2fc3bd8"
    )


def test_canonical_json_is_order_and_whitespace_stable():
    document = {
        "z": 1,
        "a": {"b": 2.0, "a": [3, {"y": 4.0, "x": 5}]},
        "unicode": "Grüße",
    }
    reordered = _reverse_object_keys(document)
    spaced = json.loads(json.dumps(document, indent=2, ensure_ascii=False))

    canonical = dp.canonical_json_bytes(document)
    assert canonical == dp.canonical_json_bytes(reordered)
    assert canonical == dp.canonical_json_bytes(spaced)
    assert b" " not in canonical
    assert b'"a":{"a":[3,{"x":5,"y":4}],"b":2}' in canonical

    # Integral floats and negative zero normalize exactly like the Rust codec.
    assert dp.canonical_json({"a": 1.0, "b": -0.0, "c": 1.5}) == '{"a":1,"b":0,"c":1.5}'

    hash_a = dp.definition_hash(document)
    hash_b = dp.definition_hash(reordered)
    assert hash_a == hash_b
    assert len(hash_a) == 32

    with pytest.raises(dp.DisplayProtocolError):
        dp.canonical_json({"bad": float("nan")})


def test_build_definition_field_ids_and_tables_are_stable():
    layout = {
        "id": "panel",
        "widgets": [
            {"type": "gauge", "metric": "@active.cpu_temp"},
            {"type": "label", "text": "{scenario_name} {node_name} {time}"},
            {"type": "bar", "metric": "node.ram_pct", "action": "org.ozma.scenario.activate"},
        ],
        "fields": {
            "node.online": {"type": "bool"},
            "node.cpu_temp": {"type": "real", "unit": "celsius"},
        },
        "actions": {
            "org.ozma.scenario.activate": {
                "params_schema": {"type": "object"},
                "permissions": ["scenario.control"],
            }
        },
    }

    first = dp.build_definition(layout, "screen-1", 3)
    second = dp.build_definition(_reverse_object_keys(layout), "screen-1", 3)

    # IDs follow an explicit allocator over sorted keys, never dict order.
    keys = [field.key for field in first.fields]
    assert keys == sorted(keys)
    assert [field.id for field in first.fields] == list(range(1, len(keys) + 1))
    assert [field.key for field in second.fields] == keys
    assert [field.id for field in second.fields] == [field.id for field in first.fields]

    assert first.id == "screen-1"
    assert first.version == 3
    assert first.hash == second.hash
    assert first.definition_json == second.definition_json
    assert first.hash == dp.definition_hash(first.definition_json)
    assert first.hash == dp.definition_hash(json.loads(first.definition_json))
    assert all(isinstance(field, data_pb2.FieldDef) for field in first.fields)

    by_key = {field.key: field for field in first.fields}
    assert by_key["node.online"].type == data_pb2.VALUE_TYPE_BOOL
    assert by_key["node.cpu_temp"].type == data_pb2.VALUE_TYPE_REAL
    assert by_key["node.cpu_temp"].unit == "celsius"
    assert by_key["scenario_name"].type == data_pb2.VALUE_TYPE_STRING
    assert by_key["node_name"].type == data_pb2.VALUE_TYPE_STRING
    assert by_key["time"].type == data_pb2.VALUE_TYPE_STRING

    assert [action.key for action in first.actions] == ["org.ozma.scenario.activate"]
    assert [action.id for action in first.actions] == [1]
    assert all(isinstance(action, action_pb2.ActionDef) for action in first.actions)
    assert json.loads(first.actions[0].params_schema_json) == {"type": "object"}
    assert list(first.actions[0].permissions) == ["scenario.control"]

    document = json.loads(first.definition_json)
    assert document["fields"] == [
        {"id": field.id, "key": field.key,
         **({"unit": field.unit} if field.HasField("unit") else {}),
         "type": dp.TYPE_NAMES[field.type]}
        for field in first.fields
    ]


def test_build_data_update_only_includes_requested_fields_and_types():
    layout = {
        "fields": {
            "node.cpu_temp": {"type": "real"},
            "node.fan_rpm": {"type": "uint"},
            "node.online": {"type": "bool"},
            "node.count": {"type": "sint"},
            "node.level_scaled": {"type": "sint", "scale": 100},
            "scenario_name": {"type": "string"},
        },
        "widgets": [
            {"type": "gauge", "metric": "node.cpu_temp"},
            {"type": "number", "metric": "node.fan_rpm"},
            {"type": "icon", "metric": "node.online"},
            {"type": "number", "metric": "node.count"},
            {"type": "label", "text": "{scenario_name}"},
        ],
    }
    definition = dp.build_definition(layout, "screen-1", 7)
    by_key = {field.key: field.id for field in definition.fields}

    update = dp.build_data_update(
        definition,
        {
            "node.cpu_temp": 65.2,
            "node.fan_rpm": 1200,
            "node.online": True,
            "scenario_name": "Gaming",
            "node.count": None,  # explicit nulls are not sent
            "not.in.definition": 999,
        },
    )
    assert update.definition_version == 7
    assert len(update.values) == 4
    assert [value.field_id for value in update.values] == sorted(
        by_key[key] for key in ("node.cpu_temp", "node.fan_rpm", "node.online", "scenario_name")
    )

    decoded = {value.field_id: value for value in update.values}
    assert decoded[by_key["node.cpu_temp"]].WhichOneof("value") == "real"
    assert decoded[by_key["node.cpu_temp"]].real == pytest.approx(65.2)
    assert decoded[by_key["node.fan_rpm"]].WhichOneof("value") == "unsigned"
    assert decoded[by_key["node.fan_rpm"]].unsigned == 1200
    assert decoded[by_key["node.online"]].WhichOneof("value") == "flag"
    assert decoded[by_key["node.online"]].flag is True
    assert decoded[by_key["scenario_name"]].WhichOneof("value") == "text"
    assert decoded[by_key["scenario_name"]].text == "Gaming"

    # The public encoder returns a complete Display frame.
    tag, framed = dp.decode_frame(dp.encode_data_update(definition, {"node.online": False}))
    assert tag == dp.TAG_DATA_UPDATE
    assert isinstance(framed, data_pb2.DataUpdate)
    assert len(framed.values) == 1
    assert framed.values[0].WhichOneof("value") == "flag"

    # Explicit field_ids restrict the update to the requested IDs.
    requested = dp.build_data_update(
        definition,
        {"node.cpu_temp": 40.0, "node.fan_rpm": 900, "scenario_name": "Idle"},
        field_ids=["node.fan_rpm"],
    )
    assert [value.field_id for value in requested.values] == [by_key["node.fan_rpm"]]
    assert requested.values[0].unsigned == 900

    # Numeric field IDs work as value keys too.
    by_id = dp.build_data_update(definition, {by_key["node.online"]: True})
    assert [value.field_id for value in by_id.values] == [by_key["node.online"]]

    # Scaled integer fields apply the declared scale to float inputs; integer
    # inputs are treated as already scaled, matching the Rust encoder.
    scaled = dp.build_data_update(
        definition,
        {"node.level_scaled": 0.42},
    )
    assert scaled.values[0].WhichOneof("value") == "integer"
    assert scaled.values[0].integer == 42
    already_scaled = dp.build_data_update(
        definition,
        {"node.level_scaled": 42},
    )
    assert already_scaled.values[0].integer == 42


def test_action_and_interaction_decoders():
    invoke = action_pb2.ActionInvoke(
        action_id=9,
        params_json=b'{"scenario": "gaming"}',
        idempotency_key=42,
    )
    framed = dp.encode_frame(dp.TAG_ACTION_INVOKE, invoke)
    decoded = dp.decode_action_invoke(framed)
    assert decoded.action_id == 9
    assert dp.action_invoke_params(decoded) == {"scenario": "gaming"}
    assert dp.action_invoke_to_dict(decoded)["idempotency_key"] == 42

    # Bare protobuf payloads are accepted as a convenience.
    assert dp.decode_action_invoke(invoke.SerializeToString()) == invoke

    event = action_pb2.InteractionEvent(node_id=5)
    event.semantic.name = "media.next"
    event.semantic.params["direction"] = "forward"
    interaction = dp.decode_interaction_event(
        dp.encode_frame(dp.TAG_INTERACTION_EVENT, event)
    )
    as_dict = dp.interaction_event_to_dict(interaction)
    assert as_dict["node_id"] == 5
    assert as_dict["event"] == "semantic"
    assert as_dict["semantic"] == {"name": "media.next", "params": {"direction": "forward"}}

    pointer_event = action_pb2.InteractionEvent(node_id=6)
    pointer_event.pointer.x = 10
    pointer_event.pointer.y = 20
    pointer_event.pointer.phase = action_pb2.POINTER_PHASE_DOWN
    pointer_dict = dp.interaction_event_to_dict(pointer_event)
    assert pointer_dict["event"] == "pointer"
    assert pointer_dict["pointer"]["phase"] == "down"

    embedded = action_pb2.InteractionEvent(node_id=7)
    embedded.action.action_id = 3
    embedded.action.params_json = b'{"on": true}'
    embedded_dict = dp.interaction_event_to_dict(embedded)
    assert embedded_dict["event"] == "action"
    assert embedded_dict["action"]["action_id"] == 3
    assert embedded_dict["action"]["params"] == {"on": True}
