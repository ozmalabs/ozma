// SPDX-License-Identifier: AGPL-3.0-only
//! State-machine tests for a client Display session.

use ozma_display_definition::{DisplayDefinitionDocument, MAX_PATCH_BYTES};
use ozma_display_protocol::{proto, DisplayEvent, DisplayMessage, DisplaySession, SessionError};
use serde_json::{json, Value};

const DEFINITION_V1: &[u8] = br#"
{
  "id": "org.ozma.test/session",
  "version": 1,
  "fields": [
    {"id": 1, "key": "org.ozma/test.temp", "type": "sint", "scale": 100},
    {"id": 2, "key": "org.ozma/test.online", "type": "bool"},
    {"id": 3, "key": "org.ozma/test.label", "type": "string"}
  ],
  "actions": [
    {
      "id": 7,
      "key": "org.ozma.scenario.activate",
      "params_schema": "{\"type\":\"object\"}",
      "permissions": ["scenario.control"]
    }
  ],
  "layout": {"type": "column", "children": []}
}
"#;

fn document_v1() -> DisplayDefinitionDocument {
    DisplayDefinitionDocument::parse(DEFINITION_V1).expect("valid definition")
}

fn definition_message(version: u32) -> proto::DisplayDefinition {
    let document = if version == 1 {
        document_v1()
    } else {
        let mut value: Value = serde_json::from_slice(DEFINITION_V1).expect("JSON object");
        value["version"] = json!(version);
        DisplayDefinitionDocument::parse(&serde_json::to_vec(&value).expect("JSON bytes"))
            .expect("valid definition")
    };
    document.to_proto().expect("compile definition")
}

fn field_value(
    document: &DisplayDefinitionDocument,
    field_id: u32,
    value: Value,
) -> proto::FieldValue {
    document
        .encode_field_value(field_id, &value)
        .expect("encode field value")
}

fn data_update(
    document: &DisplayDefinitionDocument,
    definition_version: u32,
    values: Vec<proto::FieldValue>,
) -> proto::DataUpdate {
    debug_assert_eq!(document.version(), definition_version);
    proto::DataUpdate {
        definition_version,
        values,
        sequence: Some(1),
        timestamp_micros: None,
    }
}

fn apply_definition(session: &mut DisplaySession, version: u32) {
    let event = session
        .apply(DisplayMessage::Definition(definition_message(version)))
        .expect("apply definition");
    assert_eq!(
        event,
        DisplayEvent::DefinitionApplied {
            id: "org.ozma.test/session".to_string(),
            version,
        }
    );
}

fn compressed(bytes: &[u8]) -> Vec<u8> {
    zstd::bulk::compress(bytes, 3).expect("compress test payload")
}

#[test]
fn definition_is_applied_once_and_data_updates_decode() {
    let document = document_v1();
    let mut session = DisplaySession::new(1024 * 1024);

    apply_definition(&mut session, 1);
    assert_eq!(session.definition_id(), Some("org.ozma.test/session"));
    assert_eq!(session.definition_version(), Some(1));
    assert_eq!(session.applied_hash(), Some(document.hash().as_slice()));
    assert_eq!(
        session.applied_definition_json(),
        Some(document.canonical_json().expect("canonical").as_slice())
    );

    let update = data_update(
        &document,
        1,
        vec![
            field_value(&document, 1, json!(65.5)),
            field_value(&document, 2, json!(true)),
        ],
    );
    let event = session
        .apply(DisplayMessage::DataUpdate(update.clone()))
        .expect("apply data update");
    assert_eq!(
        event,
        DisplayEvent::DataUpdated {
            definition_version: 1,
            changed_field_ids: vec![1, 2],
        }
    );
    assert_eq!(session.field_value(1), Some(&json!(6550)));
    assert_eq!(session.field_value(2), Some(&json!(true)));
    assert_eq!(session.last_data_sequence(), Some(1));

    // Re-sending identical values is accepted but reports no changes.
    let event = session
        .apply(DisplayMessage::DataUpdate(update))
        .expect("re-apply data update");
    assert_eq!(
        event,
        DisplayEvent::DataUpdated {
            definition_version: 1,
            changed_field_ids: Vec::new(),
        }
    );
}

#[test]
fn wrong_hash_definition_is_rejected_without_replacing_state() {
    let document = document_v1();
    let mut session = DisplaySession::new(1024 * 1024);
    apply_definition(&mut session, 1);

    let first_update = data_update(&document, 1, vec![field_value(&document, 1, json!(20.0))]);
    session
        .apply(DisplayMessage::DataUpdate(first_update))
        .expect("apply data update");
    assert_eq!(session.field_value(1), Some(&json!(2000)));

    let mut bad = definition_message(2);
    bad.hash = vec![0xde, 0xad, 0xbe, 0xef];
    let event = session
        .apply(DisplayMessage::Definition(bad))
        .expect("definition rejection is an event");
    assert!(matches!(
        event,
        DisplayEvent::DefinitionRejected {
            id,
            version: 2,
            reason,
        } if id == "org.ozma.test/session" && reason.contains("hash")
    ));

    assert_eq!(session.definition_version(), Some(1));
    assert_eq!(session.applied_hash(), Some(document.hash().as_slice()));
    assert_eq!(session.field_value(1), Some(&json!(2000)));

    // A version mismatch inside the JSON is also rejected.
    let mut mismatched = definition_message(1);
    let mut value: Value =
        serde_json::from_slice(&mismatched.definition_json).expect("definition JSON object");
    value["version"] = json!(3);
    mismatched.definition_json =
        serde_json::to_vec(&value).expect("serialize mismatched definition");
    mismatched.hash = DisplayDefinitionDocument::parse(&mismatched.definition_json)
        .expect("parsed document")
        .hash()
        .to_vec();

    let event = session
        .apply(DisplayMessage::Definition(mismatched))
        .expect("definition rejection is an event");
    assert!(matches!(
        event,
        DisplayEvent::DefinitionRejected {
            version: 1,
            reason,
            ..
        } if reason.contains("version")
    ));
    assert_eq!(session.definition_version(), Some(1));
    assert_eq!(session.field_value(1), Some(&json!(2000)));
}

#[test]
fn patch_is_applied_to_the_cached_definition() {
    let document = document_v1();
    let patch_json = br#"[
        {"op":"replace","path":"/version","value":2},
        {"op":"replace","path":"/layout/type","value":"row"}
    ]"#;
    let patched = document.apply_patch(patch_json).expect("apply patch");

    let mut session = DisplaySession::new(1024 * 1024);
    apply_definition(&mut session, 1);

    let update = data_update(&document, 1, vec![field_value(&document, 1, json!(12.5))]);
    session
        .apply(DisplayMessage::DataUpdate(update))
        .expect("apply data update");

    let event = session
        .apply(DisplayMessage::DefinitionPatch(proto::DefinitionPatch {
            definition_id: document.id().to_string(),
            base_version: 1,
            version: 2,
            patch_json: patch_json.to_vec(),
            hash: patched.hash().to_vec(),
            patch_zstd: None,
        }))
        .expect("patch is an event");
    assert_eq!(
        event,
        DisplayEvent::DefinitionPatched {
            id: "org.ozma.test/session".to_string(),
            from: 1,
            to: 2,
        }
    );
    assert_eq!(session.definition_version(), Some(2));
    assert_eq!(session.applied_hash(), Some(patched.hash().as_slice()));
    assert_eq!(
        session.definition().map(DisplayDefinitionDocument::version),
        Some(2)
    );
    // Field 1 still exists under the patched table, so its value is retained.
    assert_eq!(session.field_value(1), Some(&json!(1250)));
}

#[test]
fn patch_without_a_cached_base_requests_the_definition() {
    let document = document_v1();
    let patch_json = br#"[{"op":"replace","path":"/version","value":2}]"#;
    let patched = document.apply_patch(patch_json).expect("apply patch");

    let mut session = DisplaySession::new(1024 * 1024);
    let event = session
        .apply(DisplayMessage::DefinitionPatch(proto::DefinitionPatch {
            definition_id: document.id().to_string(),
            base_version: 1,
            version: 2,
            patch_json: patch_json.to_vec(),
            hash: patched.hash().to_vec(),
            patch_zstd: None,
        }))
        .expect("missing base is an event");
    assert_eq!(
        event,
        DisplayEvent::NeedDefinition {
            id: "org.ozma.test/session".to_string(),
            version: 1,
            hash: Vec::new(),
        }
    );
    assert_eq!(session.definition_version(), None);
}

#[test]
fn definition_ref_reports_cache_hit_or_need() {
    let document = document_v1();
    let mut session = DisplaySession::new(1024 * 1024);

    let event = session
        .apply(DisplayMessage::DefinitionRef(document.definition_ref()))
        .expect("definition ref is an event");
    assert_eq!(
        event,
        DisplayEvent::NeedDefinition {
            id: document.id().to_string(),
            version: 1,
            hash: document.hash().to_vec(),
        }
    );

    apply_definition(&mut session, 1);
    let event = session
        .apply(DisplayMessage::DefinitionRef(document.definition_ref()))
        .expect("definition ref is an event");
    assert_eq!(
        event,
        DisplayEvent::DefinitionCacheHit {
            id: document.id().to_string(),
            version: 1,
        }
    );

    let mut wrong_hash = document.definition_ref();
    wrong_hash.hash = vec![0xff; 32];
    let event = session
        .apply(DisplayMessage::DefinitionRef(wrong_hash))
        .expect("definition ref is an event");
    assert!(matches!(
        event,
        DisplayEvent::NeedDefinition { version: 1, .. }
    ));

    let mut wrong_version = document.definition_ref();
    wrong_version.version = 2;
    let event = session
        .apply(DisplayMessage::DefinitionRef(wrong_version))
        .expect("definition ref is an event");
    assert!(matches!(
        event,
        DisplayEvent::NeedDefinition { version: 2, .. }
    ));
}

#[test]
fn stale_scene_revisions_are_ignored() {
    let mut session = DisplaySession::new(1024 * 1024);

    let commit = proto::SceneCommit {
        revision: 10,
        operations: Vec::new(),
        base_revision: None,
        definition_id: None,
    };
    let event = session
        .apply(DisplayMessage::SceneCommit(commit))
        .expect("scene commit is an event");
    assert_eq!(event, DisplayEvent::SceneUpdated { revision: 10 });
    assert_eq!(session.scene_revision(), Some(10));
    assert_eq!(
        session.latest_scene_commit().map(|commit| commit.revision),
        Some(10)
    );

    for stale in [9, 10] {
        let event = session
            .apply(DisplayMessage::SceneCommit(proto::SceneCommit {
                revision: stale,
                operations: Vec::new(),
                base_revision: None,
                definition_id: None,
            }))
            .expect("scene commit is an event");
        assert_eq!(
            event,
            DisplayEvent::SceneIgnored {
                revision: stale,
                latest: 10,
            }
        );
    }

    assert_eq!(session.scene_revision(), Some(10));
    assert_eq!(
        session.latest_scene_commit().map(|commit| commit.revision),
        Some(10)
    );
}

#[test]
fn data_for_an_unknown_definition_version_is_rejected() {
    let document = document_v1();
    let mut session = DisplaySession::new(1024 * 1024);
    apply_definition(&mut session, 1);

    let update = proto::DataUpdate {
        definition_version: 2,
        values: vec![field_value(&document, 1, json!(1.0))],
        sequence: Some(9),
        timestamp_micros: None,
    };
    let error = session
        .apply(DisplayMessage::DataUpdate(update))
        .expect_err("wrong definition version");
    assert!(matches!(
        error,
        SessionError::DefinitionVersionMismatch {
            update_version: 2,
            cached_version: 1,
        }
    ));
    assert!(session.field_values().is_empty());
}

#[test]
fn unknown_fields_and_type_mismatches_are_rejected_atomically() {
    let document = document_v1();
    let mut session = DisplaySession::new(1024 * 1024);
    apply_definition(&mut session, 1);

    let update = data_update(
        &document,
        1,
        vec![
            field_value(&document, 1, json!(1.0)),
            proto::FieldValue {
                field_id: 99,
                value: Some(proto::field_value::Value::Flag(false)),
            },
        ],
    );
    let error = session
        .apply(DisplayMessage::DataUpdate(update))
        .expect_err("unknown field");
    assert!(matches!(error, SessionError::UnknownField(99)));
    // The valid field in the same update was not applied.
    assert!(session.field_values().is_empty());

    let update = data_update(
        &document,
        1,
        vec![
            field_value(&document, 1, json!(2.0)),
            proto::FieldValue {
                field_id: 2,
                value: Some(proto::field_value::Value::Text("not a flag".to_string())),
            },
        ],
    );
    let error = session
        .apply(DisplayMessage::DataUpdate(update))
        .expect_err("type mismatch");
    assert!(matches!(
        error,
        SessionError::InvalidFieldValue { field_id: 2, .. }
    ));
    assert!(session.field_values().is_empty());
}

#[test]
fn actions_and_interactions_do_not_touch_definition_state() {
    let mut session = DisplaySession::new(1024 * 1024);
    apply_definition(&mut session, 1);

    let invoke = proto::ActionInvoke {
        action_id: 7,
        params: br#"{"scenario":"focus"}"#.to_vec(),
        idempotency_key: 42,
        params_json: None,
        timestamp_micros: None,
    };
    let event = session
        .apply(DisplayMessage::ActionInvoke(invoke.clone()))
        .expect("action is an event");
    assert_eq!(event, DisplayEvent::Action(invoke));

    let interaction = proto::InteractionEvent {
        node_id: 1,
        target_id: Some("target".to_string()),
        timestamp_micros: Some(123),
        event: None,
    };
    let event = session
        .apply(DisplayMessage::InteractionEvent(interaction.clone()))
        .expect("interaction is an event");
    assert_eq!(event, DisplayEvent::Interaction(interaction));
    assert_eq!(session.definition_version(), Some(1));
    assert!(session.field_values().is_empty());
}

#[test]
fn compressed_definition_is_decompressed_and_applied() {
    let mut message = definition_message(1);
    let json = std::mem::take(&mut message.definition_json);
    message.definition_zstd = compressed(&json);

    let mut session = DisplaySession::new(1024 * 1024);
    let event = session
        .apply(DisplayMessage::Definition(message))
        .expect("compressed definition is an event");
    assert_eq!(
        event,
        DisplayEvent::DefinitionApplied {
            id: "org.ozma.test/session".to_string(),
            version: 1,
        }
    );
    assert_eq!(session.definition_version(), Some(1));
    assert_eq!(
        session.applied_hash(),
        Some(document_v1().hash().as_slice())
    );
}

#[test]
fn compressed_patch_is_decompressed_and_applied() {
    let document = document_v1();
    let patch_json = br#"[
        {"op":"replace","path":"/version","value":2},
        {"op":"replace","path":"/layout/type","value":"row"}
    ]"#;
    let patched = document.apply_patch(patch_json).expect("apply patch");

    let mut session = DisplaySession::new(1024 * 1024);
    apply_definition(&mut session, 1);

    let event = session
        .apply(DisplayMessage::DefinitionPatch(proto::DefinitionPatch {
            definition_id: document.id().to_string(),
            base_version: 1,
            version: 2,
            patch_json: Vec::new(),
            hash: patched.hash().to_vec(),
            patch_zstd: Some(compressed(patch_json)),
        }))
        .expect("compressed patch is an event");
    assert_eq!(
        event,
        DisplayEvent::DefinitionPatched {
            id: "org.ozma.test/session".to_string(),
            from: 1,
            to: 2,
        }
    );
    assert_eq!(session.definition_version(), Some(2));
    assert_eq!(session.applied_hash(), Some(patched.hash().as_slice()));
}

#[test]
fn oversized_compressed_definition_is_rejected_atomically() {
    let mut message = definition_message(1);
    let json = std::mem::take(&mut message.definition_json);
    message.definition_zstd = compressed(&json);

    let document = document_v1();
    let mut session = DisplaySession::new(64);
    let event = session
        .apply(DisplayMessage::Definition(message))
        .expect("oversized definition is an event");
    assert!(matches!(
        event,
        DisplayEvent::DefinitionRejected {
            version: 1,
            reason,
            ..
        } if reason.contains("exceeding") && reason.contains("64-byte session budget")
    ));
    assert_eq!(session.definition_version(), None);
    assert_eq!(session.applied_hash(), None);
    assert_ne!(session.applied_hash(), Some(document.hash().as_slice()));

    // The uncompressed path is bounded by the same budget.
    let mut message = definition_message(1);
    message.definition_json = document.canonical_json().expect("canonical JSON");
    let event = session
        .apply(DisplayMessage::Definition(message))
        .expect("oversized definition is an event");
    assert!(matches!(
        event,
        DisplayEvent::DefinitionRejected { reason, .. } if reason.contains("session budget")
    ));
    assert_eq!(session.definition_version(), None);
}

#[test]
fn oversized_compressed_patch_is_rejected_atomically() {
    let document = document_v1();
    let mut session = DisplaySession::new(1024 * 1024);
    apply_definition(&mut session, 1);

    let update = data_update(&document, 1, vec![field_value(&document, 1, json!(20.0))]);
    session
        .apply(DisplayMessage::DataUpdate(update))
        .expect("apply data update");
    let before_hash = session.applied_hash().map(<[u8]>::to_vec);

    // A patch JSON larger than MAX_PATCH_BYTES compresses to a small frame, so
    // the limit must be enforced on the decompressed bytes.
    let oversized_value = "a".repeat(MAX_PATCH_BYTES + 1);
    let patch_json =
        format!(r#"[{{"op":"replace","path":"/layout/type","value":"{oversized_value}"}}]"#);
    assert!(patch_json.len() > MAX_PATCH_BYTES);

    let event = session
        .apply(DisplayMessage::DefinitionPatch(proto::DefinitionPatch {
            definition_id: document.id().to_string(),
            base_version: 1,
            version: 2,
            patch_json: Vec::new(),
            hash: vec![0xab; 32],
            patch_zstd: Some(compressed(patch_json.as_bytes())),
        }))
        .expect("oversized patch is an event");
    assert!(matches!(
        event,
        DisplayEvent::DefinitionRejected {
            version: 2,
            reason,
            ..
        } if reason.contains("definition patch") && reason.contains("session budget")
    ));
    assert_eq!(session.definition_version(), Some(1));
    assert_eq!(session.applied_hash().map(<[u8]>::to_vec), before_hash);
    assert_eq!(session.field_value(1), Some(&json!(2000)));
}

#[test]
fn messages_without_a_json_or_zstd_payload_are_rejected() {
    let document = document_v1();
    let mut session = DisplaySession::new(1024 * 1024);
    apply_definition(&mut session, 1);

    let mut definition = definition_message(2);
    definition.definition_json.clear();
    definition.definition_zstd.clear();
    let event = session
        .apply(DisplayMessage::Definition(definition))
        .expect("empty definition is an event");
    assert!(matches!(
        event,
        DisplayEvent::DefinitionRejected {
            version: 2,
            reason,
            ..
        } if reason.contains("neither")
    ));

    let event = session
        .apply(DisplayMessage::DefinitionPatch(proto::DefinitionPatch {
            definition_id: document.id().to_string(),
            base_version: 1,
            version: 2,
            patch_json: Vec::new(),
            hash: vec![0xab; 32],
            patch_zstd: None,
        }))
        .expect("empty patch is an event");
    assert!(matches!(
        event,
        DisplayEvent::DefinitionRejected {
            version: 2,
            reason,
            ..
        } if reason.contains("neither")
    ));

    assert_eq!(session.definition_version(), Some(1));
    assert_eq!(session.applied_hash(), Some(document.hash().as_slice()));
}

#[test]
fn uncompressed_payload_wins_over_the_compressed_form() {
    let mut definition = definition_message(1);
    // Invalid zstd must be ignored because `definition_json` is present.
    definition.definition_zstd = b"not a zstd frame".to_vec();
    let mut session = DisplaySession::new(1024 * 1024);
    let event = session
        .apply(DisplayMessage::Definition(definition))
        .expect("definition is an event");
    assert_eq!(
        event,
        DisplayEvent::DefinitionApplied {
            id: "org.ozma.test/session".to_string(),
            version: 1,
        }
    );

    let document = document_v1();
    let patch_json = br#"[{"op":"replace","path":"/version","value":2}]"#;
    let patched = document.apply_patch(patch_json).expect("apply patch");
    let event = session
        .apply(DisplayMessage::DefinitionPatch(proto::DefinitionPatch {
            definition_id: document.id().to_string(),
            base_version: 1,
            version: 2,
            patch_json: patch_json.to_vec(),
            hash: patched.hash().to_vec(),
            patch_zstd: Some(b"not a zstd frame".to_vec()),
        }))
        .expect("patch is an event");
    assert_eq!(
        event,
        DisplayEvent::DefinitionPatched {
            id: "org.ozma.test/session".to_string(),
            from: 1,
            to: 2,
        }
    );
}

#[test]
fn matching_proto_tables_are_accepted() {
    let message = definition_message(1);
    assert!(!message.fields.is_empty());
    assert!(!message.actions.is_empty());

    let mut session = DisplaySession::new(1024 * 1024);
    let event = session
        .apply(DisplayMessage::Definition(message))
        .expect("matching tables are an event");
    assert_eq!(
        event,
        DisplayEvent::DefinitionApplied {
            id: "org.ozma.test/session".to_string(),
            version: 1,
        }
    );
}

#[test]
fn mismatched_field_table_is_rejected_without_replacing_state() {
    let document = document_v1();
    let mut session = DisplaySession::new(1024 * 1024);
    apply_definition(&mut session, 1);

    let mut definition = definition_message(2);
    definition.fields[0].key.push_str(".tampered");
    let event = session
        .apply(DisplayMessage::Definition(definition))
        .expect("mismatched field table is an event");
    assert!(matches!(
        event,
        DisplayEvent::DefinitionRejected {
            version: 2,
            reason,
            ..
        } if reason.contains("field table does not match")
    ));

    assert_eq!(session.definition_version(), Some(1));
    assert_eq!(session.applied_hash(), Some(document.hash().as_slice()));
    assert!(session.field_values().is_empty());
}

#[test]
fn mismatched_action_table_is_rejected_without_replacing_state() {
    let document = document_v1();
    let mut session = DisplaySession::new(1024 * 1024);
    apply_definition(&mut session, 1);

    let mut definition = definition_message(2);
    definition.actions[0]
        .permissions
        .push("org.ozma.extra.permission".to_string());
    let event = session
        .apply(DisplayMessage::Definition(definition))
        .expect("mismatched action table is an event");
    assert!(matches!(
        event,
        DisplayEvent::DefinitionRejected {
            version: 2,
            reason,
            ..
        } if reason.contains("action table does not match")
    ));

    assert_eq!(session.definition_version(), Some(1));
    assert_eq!(session.applied_hash(), Some(document.hash().as_slice()));
}

#[test]
fn empty_proto_tables_are_accepted_for_json_only_senders() {
    let mut definition = definition_message(1);
    definition.fields.clear();
    definition.actions.clear();

    let mut session = DisplaySession::new(1024 * 1024);
    let event = session
        .apply(DisplayMessage::Definition(definition))
        .expect("empty tables are an event");
    assert_eq!(
        event,
        DisplayEvent::DefinitionApplied {
            id: "org.ozma.test/session".to_string(),
            version: 1,
        }
    );
    assert!(session.definition().is_some_and(|document| {
        !document.fields().is_empty() && !document.actions().is_empty()
    }));
}
