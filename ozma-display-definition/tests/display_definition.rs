// SPDX-License-Identifier: AGPL-3.0-only

use ozma_display_definition::{
    DefinitionError, DisplayDefinitionDocument, FieldValueType, MAX_DEFINITION_BYTES,
};
use ozma_protocol_proto::v1::{field_value, FieldValue, ValueType};
use serde_json::json;

const STATUS_DEFINITION: &str = r#"
{
  "id": "org.ozma.dashboard/status",
  "version": 3,
  "fields": [
    {
      "id": 1,
      "key": "org.ozma/node.cpu_temp",
      "type": "sint",
      "unit": "celsius",
      "scale": 100
    }
  ],
  "actions": [
    {
      "id": 1,
      "key": "org.ozma.scenario.activate",
      "params_schema": "{\"type\":\"object\"}",
      "permissions": ["scenario.control"]
    }
  ],
  "layout": { "type": "column", "children": [] }
}
"#;

fn status_definition() -> DisplayDefinitionDocument {
    DisplayDefinitionDocument::parse_str(STATUS_DEFINITION).expect("sample definition is valid")
}

fn error_for(document: &str) -> DefinitionError {
    DisplayDefinitionDocument::parse_str(document).expect_err("document should be rejected")
}

#[test]
fn canonical_hash_is_stable_across_key_order_and_whitespace() {
    let reordered = br#"{
        "layout": { "children": [], "type": "column" },
        "actions": [ { "permissions": ["scenario.control"], "params_schema": "{\"type\":\"object\"}", "key": "org.ozma.scenario.activate", "id": 1 } ],
        "fields": [ { "scale": 100, "unit": "celsius", "type": "sint", "key": "org.ozma/node.cpu_temp", "id": 1 } ],
        "version": 3,
        "id": "org.ozma.dashboard/status"
    }"#;

    let whitespace = br#"{"id":"org.ozma.dashboard/status","version":3,"fields":[{"id":1,"key":"org.ozma/node.cpu_temp","type":"sint","unit":"celsius","scale":100}],"actions":[{"id":1,"key":"org.ozma.scenario.activate","params_schema":"{\"type\":\"object\"}","permissions":["scenario.control"]}],"layout":{"type":"column","children":[]}}"#;

    let a = DisplayDefinitionDocument::parse(reordered).expect("reordered document parses");
    let b = DisplayDefinitionDocument::parse(whitespace).expect("compact document parses");

    assert_eq!(
        a.canonical_json().expect("canonical JSON"),
        b.canonical_json().expect("canonical JSON")
    );
    assert_eq!(a.hash(), b.hash());

    let canonical = String::from_utf8(a.canonical_json().expect("canonical JSON"))
        .expect("canonical JSON is UTF-8");
    assert!(canonical.starts_with("{\"actions\":["));
    assert!(!canonical.contains("\n"));
    assert!(!canonical.contains(": "));
    assert!(!canonical.contains(", "));
}

#[test]
fn canonical_json_preserves_array_order() {
    let document = r#"{
        "id": "org.ozma.layout/order",
        "version": 1,
        "layout": {
            "type": "column",
            "children": [
                {"type": "text", "value": "first"},
                {"type": "text", "value": "second"}
            ]
        }
    }"#;

    let parsed = DisplayDefinitionDocument::parse_str(document).expect("document parses");
    let canonical = String::from_utf8(parsed.canonical_json().expect("canonical JSON"))
        .expect("canonical JSON is UTF-8");

    let first = canonical
        .find("\"value\":\"first\"")
        .expect("first child is present");
    let second = canonical
        .find("\"value\":\"second\"")
        .expect("second child is present");
    assert!(first < second, "array order must be preserved");
}

#[test]
fn canonical_numeric_spellings_are_stable() {
    let integer = r#"{"id":"org.ozma.test/number","version":1,"layout":{"value":1}}"#;
    let real = r#"{"id":"org.ozma.test/number","version":1,"layout":{"value":1.0}}"#;

    let a = DisplayDefinitionDocument::parse_str(integer).expect("integer document parses");
    let b = DisplayDefinitionDocument::parse_str(real).expect("real document parses");

    assert_eq!(
        a.canonical_json().expect("canonical JSON"),
        b.canonical_json().expect("canonical JSON")
    );
    assert_eq!(a.hash(), b.hash());
}

#[test]
fn duplicate_ids_empty_keys_and_missing_attributes_are_rejected() {
    assert!(matches!(
        error_for(
            r#"{
                "id": "org.ozma.test/duplicate-field",
                "version": 1,
                "fields": [
                    {"id": 1, "key": "org.ozma/one", "type": "sint"},
                    {"id": 1, "key": "org.ozma/two", "type": "uint"}
                ]
            }"#
        ),
        DefinitionError::DuplicateFieldId(1)
    ));

    assert!(matches!(
        error_for(
            r#"{
                "id": "org.ozma.test/duplicate-action",
                "version": 1,
                "actions": [
                    {"id": 7, "key": "org.ozma/one"},
                    {"id": 7, "key": "org.ozma/two"}
                ]
            }"#
        ),
        DefinitionError::DuplicateActionId(7)
    ));

    assert!(matches!(
        error_for(
            r#"{
                "id": "org.ozma.test/empty-key",
                "version": 1,
                "fields": [{"id": 1, "key": "  ", "type": "sint"}]
            }"#
        ),
        DefinitionError::EmptyAttribute("fields[].key")
    ));

    assert!(matches!(
        error_for(r#"{"version": 1}"#),
        DefinitionError::MissingAttribute("id")
    ));
    assert!(matches!(
        error_for(r#"{"id": "org.ozma.test/no-version"}"#),
        DefinitionError::MissingAttribute("version")
    ));
    assert!(matches!(
        error_for(r#"{"id": "org.ozma.test/bad-version", "version": 1.5}"#),
        DefinitionError::InvalidAttribute("version", _)
    ));
}

#[test]
fn unknown_and_case_insensitive_types_are_handled() {
    let unknown = error_for(
        r#"{
            "id": "org.ozma.test/unknown-type",
            "version": 1,
            "fields": [{"id": 1, "key": "org.ozma/value", "type": "quantum"}]
        }"#,
    );
    assert!(matches!(unknown, DefinitionError::UnknownValueType(ref name) if name == "quantum"));

    let parsed = DisplayDefinitionDocument::parse_str(
        r#"{
            "id": "org.ozma.test/case",
            "version": 1,
            "fields": [
                {"id": 1, "key": "org.ozma/a", "type": "SInT"},
                {"id": 2, "key": "org.ozma/b", "type": "TIMESTAMP"},
                {"id": 3, "key": "org.ozma/c", "type": "Bool"}
            ]
        }"#,
    )
    .expect("case-insensitive types parse");

    assert_eq!(parsed.fields()[0].value_type, "sint");
    assert_eq!(parsed.fields()[1].value_type, "timestamp");
    assert_eq!(parsed.fields()[2].value_type, "bool");
    assert_eq!(
        ozma_display_definition::field_value_type(&parsed.fields()[0]).expect("resolvable type"),
        FieldValueType::Sint
    );
}

#[test]
fn size_limit_is_enforced() {
    let oversized = vec![b' '; MAX_DEFINITION_BYTES + 1];
    assert!(matches!(
        DisplayDefinitionDocument::parse(&oversized),
        Err(DefinitionError::DocumentTooLarge { .. })
    ));
}

#[test]
fn nested_patch_applies_and_updates_the_document() {
    let original = status_definition();
    let original_hash = original.hash();

    let patch = br#"[
        {"op": "replace", "path": "/version", "value": 4},
        {"op": "add", "path": "/layout/children/0", "value": {"type": "text", "text": "CPU"}},
        {"op": "add", "path": "/fields/-", "value": {"id": 2, "key": "org.ozma/node.load", "type": "real"}}
    ]"#;

    let patched = original.apply_patch(patch).expect("patch applies");
    assert_eq!(patched.version(), 4);
    assert_eq!(patched.fields().len(), 2);
    assert_eq!(patched.fields()[1].id, 2);
    assert_eq!(patched.layout()["children"][0]["text"], "CPU");
    assert_ne!(patched.hash(), original_hash);
    assert_eq!(original.version(), 3, "the original document is unchanged");
    assert_eq!(original.fields().len(), 1);

    let invalid = original
        .apply_patch(br#"[{"op": "replace", "path": "/fields/0/type", "value": "quantum"}]"#);
    assert!(matches!(invalid, Err(DefinitionError::UnknownValueType(_))));

    let missing = original.apply_patch(br#"[{"op": "remove", "path": "/layout/nope"}]"#);
    assert!(matches!(missing, Err(DefinitionError::InvalidPatch(_))));
}

#[test]
fn to_proto_contains_matching_tables_hash_and_canonical_bytes() {
    let document = status_definition();
    let proto = document.to_proto().expect("document compiles");

    assert_eq!(proto.id, document.id());
    assert_eq!(proto.version, 3);
    assert_eq!(proto.hash, document.hash().to_vec());
    assert_eq!(
        proto.definition_json,
        document.canonical_json().expect("canonical JSON")
    );
    assert!(proto.definition_zstd.is_empty());

    assert_eq!(proto.fields.len(), document.fields().len());
    assert_eq!(proto.fields[0].id, 1);
    assert_eq!(proto.fields[0].key, "org.ozma/node.cpu_temp");
    assert_eq!(proto.fields[0].r#type, ValueType::Sint as i32);
    assert_eq!(proto.fields[0].unit.as_deref(), Some("celsius"));
    assert_eq!(proto.fields[0].scale, Some(100));

    assert_eq!(proto.actions.len(), document.actions().len());
    assert_eq!(proto.actions[0].id, 1);
    assert_eq!(proto.actions[0].key, "org.ozma.scenario.activate");
    assert_eq!(
        proto.actions[0].params_schema_json,
        br#"{"type":"object"}"#.to_vec()
    );
    assert_eq!(proto.actions[0].permissions, vec!["scenario.control"]);
    assert!(!proto.actions[0].requires_confirmation);

    let reference = document.definition_ref();
    assert_eq!(reference.id, document.id());
    assert_eq!(reference.version, document.version());
    assert_eq!(reference.hash, document.hash().to_vec());
}

fn value_definition() -> DisplayDefinitionDocument {
    DisplayDefinitionDocument::parse_str(
        r#"{
            "id": "org.ozma.test/values",
            "version": 1,
            "fields": [
                {"id": 1, "key": "org.ozma/sint", "type": "sint", "unit": "celsius", "scale": 100},
                {"id": 2, "key": "org.ozma/uint", "type": "uint"},
                {"id": 3, "key": "org.ozma/real", "type": "real"},
                {"id": 4, "key": "org.ozma/bool", "type": "bool"},
                {"id": 5, "key": "org.ozma/string", "type": "string"},
                {"id": 6, "key": "org.ozma/timestamp", "type": "timestamp"},
                {"id": 7, "key": "org.ozma/enum", "type": "enum"},
                {"id": 8, "key": "org.ozma/bytes", "type": "bytes"}
            ]
        }"#,
    )
    .expect("value definition parses")
}

fn round_trip(document: &DisplayDefinitionDocument, field_id: u32, value: serde_json::Value) {
    let encoded = document
        .encode_field_value(field_id, &value)
        .expect("value encodes");
    let decoded = document
        .decode_field_value(&encoded)
        .expect("value decodes");
    assert_eq!(decoded, value, "field {field_id} round-trips");
}

#[test]
fn field_values_encode_and_decode_for_every_type() {
    let document = value_definition();

    assert_eq!(
        document
            .encode_field_value(1, &json!(2350))
            .expect("sint encodes"),
        FieldValue {
            field_id: 1,
            value: Some(field_value::Value::Integer(2350)),
        }
    );
    round_trip(&document, 1, json!(2350));

    // A floating-point physical value is scaled to hundredths.
    assert_eq!(
        document
            .encode_field_value(1, &json!(23.5))
            .expect("scaled float encodes"),
        FieldValue {
            field_id: 1,
            value: Some(field_value::Value::Integer(2350)),
        }
    );
    assert!(document.encode_field_value(1, &json!(23.555)).is_err());
    assert!(document.encode_field_value(1, &json!("2350")).is_err());

    round_trip(&document, 2, json!(42));
    assert!(document.encode_field_value(2, &json!(-1)).is_err());
    assert!(document
        .encode_field_value(2, &json!(u64::MAX))
        .expect("u64 fits")
        .value
        .is_some());

    round_trip(&document, 3, json!(1.5));
    round_trip(&document, 4, json!(true));
    round_trip(&document, 5, json!("ok"));
    round_trip(&document, 6, json!(1_700_000_000_000_000_i64));
    round_trip(&document, 7, json!(2));
    round_trip(&document, 8, json!("hello"));

    assert_eq!(
        document
            .encode_field_value(7, &json!(2))
            .expect("enum encodes"),
        FieldValue {
            field_id: 7,
            value: Some(field_value::Value::EnumValue(2)),
        }
    );
    assert_eq!(
        document
            .encode_field_value(6, &json!(1_700_000_000_000_000_i64))
            .expect("timestamp encodes"),
        FieldValue {
            field_id: 6,
            value: Some(field_value::Value::TimestampMicros(1_700_000_000_000_000)),
        }
    );
    assert_eq!(
        document
            .encode_field_value(8, &json!("hello"))
            .expect("bytes encode"),
        FieldValue {
            field_id: 8,
            value: Some(field_value::Value::Blob(b"hello".to_vec())),
        }
    );
}

#[test]
fn field_value_type_mismatches_are_rejected() {
    let document = value_definition();

    assert!(matches!(
        document.encode_field_value(4, &json!(1)),
        Err(DefinitionError::FieldTypeMismatch {
            field_id: 4,
            expected: "bool",
            ..
        })
    ));
    assert!(matches!(
        document.encode_field_value(5, &json!(5)),
        Err(DefinitionError::FieldTypeMismatch {
            field_id: 5,
            expected: "string",
            ..
        })
    ));
    assert!(matches!(
        document.encode_field_value(99, &json!(1)),
        Err(DefinitionError::UnknownFieldId(99))
    ));

    let wrong_variant = FieldValue {
        field_id: 1,
        value: Some(field_value::Value::Unsigned(1)),
    };
    assert!(matches!(
        document.decode_field_value(&wrong_variant),
        Err(DefinitionError::FieldTypeMismatch {
            field_id: 1,
            expected: "sint",
            ..
        })
    ));

    let empty = FieldValue {
        field_id: 1,
        value: None,
    };
    assert!(matches!(
        document.decode_field_value(&empty),
        Err(DefinitionError::InvalidFieldValue { field_id: 1, .. })
    ));
}

#[test]
fn canonical_fixture_matches_the_controller_language_vector() {
    // Shared with tests/unit/controller/test_display_protocol.py. If either
    // canonicalizer changes, this test and the Python test fail together.
    const FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/display_definition.json");
    const EXPECTED_HASH: &str = "6afa1c4d445e67f71df2f978e029ad2c8399687d443bb7ce4a8436d5d2fc3bd8";

    let document = DisplayDefinitionDocument::parse(FIXTURE).expect("fixture parses");
    let canonical = String::from_utf8(document.canonical_json().unwrap()).unwrap();
    assert!(canonical.contains(r#""numbers":[1.5,65.2,1e20,1e-7,0]"#));
    assert!(canonical.contains(r#""unicode":"Grüße""#));

    let actual: String = document
        .hash()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(actual, EXPECTED_HASH);
}
