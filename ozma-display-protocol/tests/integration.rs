// SPDX-License-Identifier: AGPL-3.0-only
//! End-to-end checks across canonical definitions, protobuf messages, and the
//! Display-channel codec.

use ozma_display_definition::DisplayDefinitionDocument;
use ozma_display_protocol::scene_convert::{scene_commit_from_proto, scene_commit_to_proto};
use ozma_display_protocol::{proto, DisplayMessage};
use ozma_protocol::scene::{ImagePlacement, SceneCommit};
use serde_json::json;

const DEFINITION: &[u8] = br#"
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
    },
    {
      "id": 2,
      "key": "org.ozma/node.online",
      "type": "bool"
    }
  ],
  "actions": [
    {
      "id": 7,
      "key": "org.ozma.scenario.activate",
      "params_schema": "{\"type\":\"object\"}",
      "permissions": ["scenario.control"]
    }
  ],
  "layout": {
    "type": "column",
    "children": [
      {"type": "text", "field": 1}
    ]
  }
}
"#;

#[test]
fn definition_compiles_encodes_and_decodes() {
    let document = DisplayDefinitionDocument::parse(DEFINITION).expect("valid definition");
    let definition = document.to_proto().expect("compile definition");

    assert_eq!(definition.id, document.id());
    assert_eq!(definition.version, document.version());
    assert_eq!(definition.hash, document.hash().to_vec());
    assert_eq!(definition.fields.len(), 2);
    assert_eq!(definition.actions.len(), 1);
    assert_eq!(definition.actions[0].id, 7);

    let message = DisplayMessage::Definition(definition);
    let encoded = message.encode().expect("encode definition");
    let decoded = DisplayMessage::decode(&encoded).expect("decode definition");
    assert_eq!(decoded, message);
}

#[test]
fn typed_values_travel_in_a_data_update() {
    let document = DisplayDefinitionDocument::parse(DEFINITION).expect("valid definition");
    let temperature = document
        .encode_field_value(1, &json!(65.5))
        .expect("encode temperature");
    let online = document
        .encode_field_value(2, &json!(true))
        .expect("encode online flag");

    let update = proto::DataUpdate {
        definition_version: document.version(),
        values: vec![temperature, online],
        ..Default::default()
    };
    let message = DisplayMessage::DataUpdate(update);
    let encoded = message.encode().expect("encode data update");
    let decoded = DisplayMessage::decode(&encoded).expect("decode data update");

    let DisplayMessage::DataUpdate(decoded) = decoded else {
        panic!("decoded a different message kind");
    };
    assert_eq!(decoded.definition_version, document.version());
    assert_eq!(decoded.values.len(), 2);
    // Scaled integers stay exact on the wire; the definition's `scale: 100`
    // tells a renderer that 6550 means 65.50 degrees.
    assert_eq!(
        document
            .decode_field_value(&decoded.values[0])
            .expect("decode temperature"),
        json!(6550)
    );
    assert_eq!(
        document
            .decode_field_value(&decoded.values[1])
            .expect("decode online flag"),
        json!(true)
    );
}

#[test]
fn definition_ref_roundtrips_through_the_display_codec() {
    let document = DisplayDefinitionDocument::parse(DEFINITION).expect("valid definition");
    let reference = document.definition_ref();
    let message = DisplayMessage::DefinitionRef(reference.clone());
    let decoded =
        DisplayMessage::decode(&message.encode().expect("encode ref")).expect("decode ref");

    assert_eq!(decoded, DisplayMessage::DefinitionRef(reference));
}

#[test]
fn domain_scene_travels_as_a_scene_snapshot() {
    let commit = SceneCommit {
        revision: 11,
        images: vec![
            ImagePlacement {
                node_id: 3,
                resource_id: 0xdead_beef,
                x: 12,
                y: -8,
                width: 320,
                height: 200,
            },
            ImagePlacement {
                node_id: 4,
                resource_id: 7,
                x: 400,
                y: 16,
                width: 96,
                height: 96,
            },
        ],
    };
    let snapshot = scene_commit_to_proto(&commit, 2).expect("encode scene commit");
    assert_eq!(snapshot.revision, commit.revision);

    let message = DisplayMessage::from(snapshot);
    let decoded = DisplayMessage::decode(&message.encode().expect("encode scene snapshot"))
        .expect("decode scene snapshot");

    let DisplayMessage::SceneSnapshot(snapshot) = decoded else {
        panic!("decoded a different message kind");
    };
    assert_eq!(snapshot.roots, vec![0]);
    assert_eq!(snapshot.nodes.len(), 3);
    assert_eq!(
        scene_commit_from_proto(&snapshot).expect("decode domain scene"),
        commit
    );
}
