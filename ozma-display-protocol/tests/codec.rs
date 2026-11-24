// SPDX-License-Identifier: AGPL-3.0-only
//! Wire-codec tests for the reliable Display channel.

use ozma_display_protocol::{
    proto, CodecError, DisplayMessage, DisplayMessageKind, MAX_FRAME_LEN, MAX_PAYLOAD_LEN,
    TAG_ACTION_INVOKE, TAG_DATA_UPDATE, TAG_DEFINITION, TAG_DEFINITION_PATCH, TAG_DEFINITION_REF,
    TAG_INTERACTION_EVENT, TAG_SCENE_COMMIT, TAG_SCENE_SNAPSHOT,
};
use prost::Message;
use proto::{
    field_value, interaction_event, scene_node, scene_op, ActionDef, ActionInvoke, DataBinding,
    DefinitionPatch, DefinitionRef, DisplayDefinition, FieldDef, FieldValue, InteractionEvent,
    InteractionTarget, KeyEvent, PointerEvent, Rect, ResourceRef, SceneCommit, SceneNode, SceneOp,
    SceneSnapshot, TextContent, Transform, ValueType, VideoSurface, WidgetNode,
};

fn sample_definition() -> DisplayDefinition {
    DisplayDefinition {
        id: "org.ozma.dashboard".to_string(),
        version: 7,
        hash: vec![0xaa, 0xbb, 0xcc, 0xdd],
        definition_json:
            br#"{"schema":1,"title":"Node dashboard","layout":{"type":"column"},"fields":[...]}"#
                .to_vec(),
        definition_zstd: vec![0x28, 0xb5, 0x2f, 0xfd, 0x04],
        fields: vec![
            FieldDef {
                id: 1,
                key: "org.ozma/node.cpu_temp".to_string(),
                r#type: ValueType::Real as i32,
                unit: Some("celsius".to_string()),
                scale: Some(100),
                description: Some("CPU package temperature".to_string()),
            },
            FieldDef {
                id: 2,
                key: "org.ozma/node.fan_rpm".to_string(),
                r#type: ValueType::Uint as i32,
                unit: Some("rpm".to_string()),
                scale: None,
                description: None,
            },
        ],
        actions: vec![sample_action_def()],
    }
}

fn sample_action_def() -> ActionDef {
    ActionDef {
        id: 7,
        key: "org.ozma.scenario.activate".to_string(),
        params_schema_json: br#"{"type":"object","required":["scenario"]}"#.to_vec(),
        permissions: vec!["scenario.control".to_string()],
        requires_confirmation: true,
        label: Some("Activate scenario".to_string()),
        description: Some("Switch the active scenario".to_string()),
    }
}

fn sample_definition_ref() -> DefinitionRef {
    DefinitionRef {
        id: "org.ozma.dashboard".to_string(),
        version: 7,
        hash: vec![0xaa, 0xbb, 0xcc, 0xdd],
    }
}

fn sample_definition_patch() -> DefinitionPatch {
    DefinitionPatch {
        definition_id: "org.ozma.dashboard".to_string(),
        base_version: 7,
        version: 8,
        patch_json: br#"[{"op":"replace","path":"/title","value":"Updated dashboard"}]"#.to_vec(),
        hash: vec![0x11, 0x22, 0x33, 0x44],
        patch_zstd: Some(vec![0x28, 0xb5, 0x2f, 0x00]),
    }
}

fn sample_scene_commit() -> SceneCommit {
    SceneCommit {
        revision: 42,
        operations: vec![
            SceneOp {
                node_id: 1,
                operation: Some(scene_op::Operation::Upsert(SceneNode {
                    id: 1,
                    parent: None,
                    transform: Some(Transform {
                        translation_x: 0,
                        translation_y: 0,
                        width: 320,
                        height: 64,
                        scale_x_ppm: 1_000_000,
                        scale_y_ppm: 1_000_000,
                        rotation_millidegrees: 0,
                        origin: None,
                        opacity_ppm: Some(1_000_000),
                    }),
                    clip: Some(Rect {
                        x: 0,
                        y: 0,
                        width: 320,
                        height: 64,
                    }),
                    z: 10,
                    content: Some(scene_node::Content::Text(TextContent {
                        text: "CPU".to_string(),
                        style: Some("label".to_string()),
                        align: Some("start".to_string()),
                        max_lines: Some(1),
                        wrap: Some(false),
                    })),
                    interaction: Some(InteractionTarget {
                        action_id: Some(7),
                        node_id: Some(1),
                        accessibility_label: Some("CPU temperature".to_string()),
                        cursor: Some("pointer".to_string()),
                    }),
                    visible: Some(true),
                    name: Some("cpu-label".to_string()),
                })),
            },
            SceneOp {
                node_id: 2,
                operation: Some(scene_op::Operation::Upsert(SceneNode {
                    id: 2,
                    parent: Some(1),
                    transform: Some(Transform::default()),
                    clip: None,
                    z: 0,
                    content: Some(scene_node::Content::Widget(WidgetNode {
                        widget: "gauge".to_string(),
                        definition_version: 7,
                        properties: [("unit".to_string(), "celsius".to_string())]
                            .into_iter()
                            .collect(),
                        bindings: vec![DataBinding {
                            definition_id: "org.ozma.dashboard".to_string(),
                            field_id: 1,
                            field_key: Some("org.ozma/node.cpu_temp".to_string()),
                            property: Some("value".to_string()),
                            format: Some("fixed(1)".to_string()),
                        }],
                    })),
                    interaction: None,
                    visible: None,
                    name: None,
                })),
            },
            SceneOp {
                node_id: 3,
                operation: Some(scene_op::Operation::Move(proto::NodeMove {
                    parent: Some(1),
                    index: Some(0),
                })),
            },
            SceneOp {
                node_id: 4,
                operation: Some(scene_op::Operation::Remove(true)),
            },
        ],
        base_revision: Some(41),
        definition_id: Some("org.ozma.dashboard".to_string()),
    }
}

fn sample_scene_snapshot() -> SceneSnapshot {
    SceneSnapshot {
        revision: 42,
        nodes: vec![
            SceneNode {
                id: 0,
                content: Some(scene_node::Content::Video(VideoSurface {
                    id: 1,
                    channel: 2,
                    width: Some(1920),
                    height: Some(1080),
                    codec: Some("h264".to_string()),
                    track_id: Some("primary-video".to_string()),
                })),
                ..Default::default()
            },
            SceneNode {
                id: 1,
                parent: Some(0),
                transform: Some(Transform {
                    translation_x: 10,
                    translation_y: 20,
                    width: 320,
                    height: 200,
                    scale_x_ppm: 1_000_000,
                    scale_y_ppm: 1_000_000,
                    opacity_ppm: Some(1_000_000),
                    ..Default::default()
                }),
                z: 0,
                content: Some(scene_node::Content::Image(ResourceRef {
                    id: 9001,
                    ..Default::default()
                })),
                ..Default::default()
            },
            SceneNode {
                id: 2,
                parent: Some(0),
                transform: Some(Transform {
                    translation_x: 350,
                    translation_y: 20,
                    width: 64,
                    height: 64,
                    scale_x_ppm: 1_000_000,
                    scale_y_ppm: 1_000_000,
                    opacity_ppm: Some(1_000_000),
                    ..Default::default()
                }),
                z: 1,
                content: Some(scene_node::Content::Image(ResourceRef {
                    id: 9002,
                    mime_type: Some("image/png".to_string()),
                    ..Default::default()
                })),
                ..Default::default()
            },
        ],
        roots: vec![0],
        base_revision: Some(41),
        definition_id: Some("org.ozma.dashboard".to_string()),
    }
}

fn sample_data_update() -> proto::DataUpdate {
    proto::DataUpdate {
        definition_version: 7,
        values: vec![
            FieldValue {
                field_id: 1,
                value: Some(field_value::Value::Real(42.5)),
            },
            FieldValue {
                field_id: 2,
                value: Some(field_value::Value::Unsigned(1_200)),
            },
            FieldValue {
                field_id: 3,
                value: Some(field_value::Value::Integer(-9)),
            },
            FieldValue {
                field_id: 4,
                value: Some(field_value::Value::Text("ready".to_string())),
            },
            FieldValue {
                field_id: 5,
                value: Some(field_value::Value::TimestampMicros(1_700_000_000_000_000)),
            },
        ],
        sequence: Some(99),
        timestamp_micros: Some(1_700_000_000_000_001),
    }
}

fn sample_action_invoke() -> ActionInvoke {
    ActionInvoke {
        action_id: 7,
        params: vec![0x08, 0x01],
        idempotency_key: 0x0123_4567_89ab_cdef,
        params_json: Some(br#"{"scenario":"focus"}"#.to_vec()),
        timestamp_micros: Some(1_700_000_000_000_002),
    }
}

fn sample_interaction_event() -> InteractionEvent {
    InteractionEvent {
        node_id: 1,
        target_id: Some("org.ozma.scenario.activate".to_string()),
        timestamp_micros: Some(1_700_000_000_000_003),
        event: Some(interaction_event::Event::Pointer(PointerEvent {
            x: 160,
            y: 32,
            phase: proto::PointerPhase::Up as i32,
            button: Some(1),
            pressure_ppt: Some(850),
            wheel_delta: None,
        })),
    }
}

fn sample_messages() -> Vec<DisplayMessage> {
    vec![
        DisplayMessage::from(sample_definition()),
        DisplayMessage::from(sample_definition_ref()),
        DisplayMessage::from(sample_definition_patch()),
        DisplayMessage::from(sample_scene_commit()),
        DisplayMessage::from(sample_data_update()),
        DisplayMessage::from(sample_action_invoke()),
        DisplayMessage::from(sample_interaction_event()),
        DisplayMessage::from(sample_scene_snapshot()),
    ]
}

#[test]
fn every_variant_roundtrips() {
    let messages = sample_messages();
    assert_eq!(messages.len(), 8, "one sample per message kind");

    for message in messages {
        let frame = message.encode().expect("encode display frame");

        assert_eq!(frame[0], message.tag());
        assert_eq!(frame.len(), message.encoded_len());
        assert_eq!(frame.len(), 1 + message.payload_len());

        let decoded = DisplayMessage::decode(&frame).expect("decode display frame");
        assert_eq!(decoded, message);
        assert_eq!(decoded.kind(), message.kind());
        assert_eq!(decoded.encode().expect("re-encode display frame"), frame);
    }
}

#[test]
fn tags_are_stable_wire_values() {
    let expected = [
        (DisplayMessageKind::Definition, 0x01, TAG_DEFINITION),
        (DisplayMessageKind::DefinitionRef, 0x02, TAG_DEFINITION_REF),
        (
            DisplayMessageKind::DefinitionPatch,
            0x03,
            TAG_DEFINITION_PATCH,
        ),
        (DisplayMessageKind::SceneCommit, 0x04, TAG_SCENE_COMMIT),
        (DisplayMessageKind::DataUpdate, 0x05, TAG_DATA_UPDATE),
        (DisplayMessageKind::ActionInvoke, 0x06, TAG_ACTION_INVOKE),
        (
            DisplayMessageKind::InteractionEvent,
            0x07,
            TAG_INTERACTION_EVENT,
        ),
        (DisplayMessageKind::SceneSnapshot, 0x08, TAG_SCENE_SNAPSHOT),
    ];

    for (kind, expected_tag, constant) in expected {
        assert_eq!(kind.as_tag(), expected_tag);
        assert_eq!(constant, expected_tag);
        assert_eq!(u8::from(kind), expected_tag);
        assert_eq!(DisplayMessageKind::from_tag(expected_tag), Some(kind));
        assert_eq!(DisplayMessageKind::try_from(expected_tag), Ok(kind));
    }

    assert_eq!(
        DisplayMessageKind::from_tag(0x00),
        None,
        "zero is not a message tag"
    );
    assert_eq!(DisplayMessageKind::from_tag(0x09), None);
    assert_eq!(DisplayMessageKind::from_tag(0xff), None);
    assert_eq!(DisplayMessageKind::try_from(0xff), Err(0xff));
}

#[test]
fn message_kind_matches_variant() {
    let kinds: Vec<DisplayMessageKind> =
        sample_messages().iter().map(DisplayMessage::kind).collect();
    assert_eq!(
        kinds,
        vec![
            DisplayMessageKind::Definition,
            DisplayMessageKind::DefinitionRef,
            DisplayMessageKind::DefinitionPatch,
            DisplayMessageKind::SceneCommit,
            DisplayMessageKind::DataUpdate,
            DisplayMessageKind::ActionInvoke,
            DisplayMessageKind::InteractionEvent,
            DisplayMessageKind::SceneSnapshot,
        ]
    );
}

#[test]
fn frame_has_one_tag_byte_and_no_length_prefix() {
    let payload = sample_action_invoke();
    let message = DisplayMessage::from(payload.clone());
    let frame = message.encode().expect("encode display frame");

    assert_eq!(frame[0], TAG_ACTION_INVOKE);
    assert_eq!(
        &frame[1..],
        payload.encode_to_vec().as_slice(),
        "the remainder is exactly the prost payload; there is no extra length prefix"
    );
    assert_eq!(frame.len(), 1 + payload.encoded_len());
    assert_eq!(MAX_FRAME_LEN, MAX_PAYLOAD_LEN + 1);
}

#[test]
fn unknown_tag_is_rejected() {
    for tag in [0x00_u8, 0x09, 0xff] {
        assert!(
            matches!(
                DisplayMessage::decode(&[tag]),
                Err(CodecError::UnknownTag(actual)) if actual == tag
            ),
            "tag 0x{tag:02x} must be rejected"
        );
    }
}

#[test]
fn empty_frame_is_rejected() {
    assert!(matches!(
        DisplayMessage::decode(&[]),
        Err(CodecError::EmptyFrame)
    ));
}

#[test]
fn truncated_frame_is_rejected() {
    // Field 1 of ActionInvoke is a varint with no value.
    assert!(matches!(
        DisplayMessage::decode(&[TAG_ACTION_INVOKE, 0x08]),
        Err(CodecError::Decode {
            kind: DisplayMessageKind::ActionInvoke,
            ..
        })
    ));

    // Field 4 of DisplayDefinition claims five bytes but only two are present.
    assert!(matches!(
        DisplayMessage::decode(&[TAG_DEFINITION, 0x22, 0x05, b'a', b'b']),
        Err(CodecError::Decode {
            kind: DisplayMessageKind::Definition,
            ..
        })
    ));

    // Damage a real frame near the end of its last length-delimited field.
    let mut frame = sample_definition().encode_to_vec();
    frame.truncate(frame.len() - 2);
    let mut wire = Vec::with_capacity(frame.len() + 1);
    wire.push(TAG_DEFINITION);
    wire.extend_from_slice(&frame);
    assert!(matches!(
        DisplayMessage::decode(&wire),
        Err(CodecError::Decode {
            kind: DisplayMessageKind::Definition,
            ..
        })
    ));
}

#[test]
fn oversized_payload_is_rejected_on_decode() {
    let mut frame = vec![0_u8; MAX_PAYLOAD_LEN + 2];
    frame[0] = TAG_DATA_UPDATE;

    assert!(frame.len() > MAX_FRAME_LEN);
    assert!(matches!(
        DisplayMessage::decode(&frame),
        Err(CodecError::OversizedPayload {
            limit: MAX_PAYLOAD_LEN,
            actual,
        }) if actual == MAX_PAYLOAD_LEN + 1
    ));
}

#[test]
fn oversized_payload_is_rejected_on_encode() {
    let message = DisplayMessage::Definition(DisplayDefinition {
        id: "org.ozma.oversized".to_string(),
        version: 1,
        hash: Vec::new(),
        definition_json: vec![b' '; MAX_PAYLOAD_LEN],
        definition_zstd: Vec::new(),
        fields: Vec::new(),
        actions: Vec::new(),
    });

    assert!(message.payload_len() > MAX_PAYLOAD_LEN);
    assert!(matches!(
        message.encode(),
        Err(CodecError::OversizedPayload {
            limit: MAX_PAYLOAD_LEN,
            actual,
        }) if actual == message.payload_len()
    ));
}

#[test]
fn decode_matches_encoded_message_exactly() {
    let message = DisplayMessage::from(sample_scene_commit());
    let frame = message.encode().expect("encode scene commit");
    let decoded = DisplayMessage::decode(&frame).expect("decode scene commit");

    match &decoded {
        DisplayMessage::SceneCommit(scene) => {
            assert_eq!(scene.revision, 42);
            assert_eq!(scene.base_revision, Some(41));
            assert_eq!(scene.operations.len(), 4);
            assert_eq!(scene.operations[0].node_id, 1);
            assert!(matches!(
                scene.operations[0].operation,
                Some(scene_op::Operation::Upsert(_))
            ));
            assert!(matches!(
                scene.operations[3].operation,
                Some(scene_op::Operation::Remove(true))
            ));
        }
        other => panic!("decoded the wrong variant: {other:?}"),
    }
    assert_eq!(decoded, message);
    assert_eq!(decoded.encode().expect("re-encode"), frame);
}

#[test]
fn scene_snapshot_roundtrips_with_its_stable_tag() {
    let snapshot = sample_scene_snapshot();
    let message = DisplayMessage::from(snapshot.clone());

    assert_eq!(message.kind(), DisplayMessageKind::SceneSnapshot);
    assert_eq!(message.tag(), TAG_SCENE_SNAPSHOT);
    assert_eq!(message.tag(), 0x08);

    let frame = message.encode().expect("encode scene snapshot");
    assert_eq!(frame[0], 0x08);
    assert_eq!(
        &frame[1..],
        snapshot.encode_to_vec().as_slice(),
        "the payload is exactly the prost SceneSnapshot"
    );

    let decoded = DisplayMessage::decode(&frame).expect("decode scene snapshot");
    assert_eq!(decoded, DisplayMessage::SceneSnapshot(snapshot));
    assert_eq!(decoded.encode().expect("re-encode"), frame);
}

#[test]
fn key_and_action_interactions_roundtrip() {
    let key_event = InteractionEvent {
        node_id: 9,
        target_id: None,
        timestamp_micros: None,
        event: Some(interaction_event::Event::Key(KeyEvent {
            key: "Enter".to_string(),
            action: proto::KeyAction::Press as i32,
            modifiers: vec!["ctrl".to_string(), "shift".to_string()],
            repeat: Some(2),
        })),
    };
    let action_event = InteractionEvent {
        node_id: 10,
        target_id: Some("org.ozma.scenario.activate".to_string()),
        timestamp_micros: Some(1),
        event: Some(interaction_event::Event::Action(sample_action_invoke())),
    };

    for event in [key_event, action_event] {
        let message = DisplayMessage::from(event);
        let frame = message.encode().expect("encode interaction");
        assert_eq!(frame[0], TAG_INTERACTION_EVENT);
        assert_eq!(
            DisplayMessage::decode(&frame).expect("decode interaction"),
            message
        );
    }
}
