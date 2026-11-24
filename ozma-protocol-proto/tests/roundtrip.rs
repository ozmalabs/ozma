// SPDX-License-Identifier: AGPL-3.0-only
//! Round-trip tests for the generated protobuf bindings.

use ozma_protocol_proto::v1::{
    field_value, scene_node, scene_op, ApiError, DataBinding, DataField, DataSchema, DataUpdate,
    DefinitionPatch, DefinitionRef, DisplayCapabilities, DisplayDefinition, DisplayMethod,
    FieldDef, FieldValue, Hello, HelloAck, Interaction, InteractionTarget, MethodLocation,
    NodeMove, ProtocolVersion, Rect, RenderingTier, SceneCommit, SceneNode, SceneOp, SceneSnapshot,
    Session, TextContent, Transform, ValueType, VideoSurface, WidgetNode, WireFormat,
};
use prost::Message;

fn roundtrip<M>(message: &M)
where
    M: Message + Default + PartialEq + std::fmt::Debug,
{
    let encoded = message.encode_to_vec();
    let decoded = M::decode(encoded.as_slice()).expect("decode generated message");
    assert_eq!(message, &decoded);
}

fn sample_hello() -> Hello {
    Hello {
        session_id: "sess-01".to_string(),
        token: "test-token".to_string(),
        protocol: Some(ProtocolVersion { major: 1, minor: 0 }),
        wire_formats: vec![WireFormat::Protobuf as i32, WireFormat::Json as i32],
        display: Some(DisplayCapabilities {
            scene_versions: vec![1],
            methods: vec![DisplayMethod {
                id: "device.scene".to_string(),
                accepts: vec!["application/vnd.ozma.scene+json".to_string()],
                schemas: vec!["org.ozma.telemetry".to_string()],
                location: MethodLocation::Device as i32,
                outputs: vec!["application/vnd.ozma.scene+json".to_string()],
                interactions: vec![Interaction::Touch as i32, Interaction::Keyboard as i32],
                configuration_schema: Some("{}".to_string()),
            }],
            tiers: vec![
                RenderingTier::NativeWidgets as i32,
                RenderingTier::DataDriven as i32,
            ],
            schemas: vec!["org.ozma.telemetry".to_string()],
            max_update_rate_hz: Some(60),
            resource_budget_bytes: 16 * 1024 * 1024,
        }),
        client_id: Some("client-01".to_string()),
        capabilities: vec!["display".to_string(), "input".to_string()],
        definitions: vec![DefinitionRef {
            id: "org.ozma.dashboard".to_string(),
            version: 4,
            hash: vec![0xaa, 0xbb, 0xcc],
        }],
    }
}

fn sample_data_schema() -> DataSchema {
    DataSchema {
        id: "org.ozma.telemetry".to_string(),
        fields: vec![DataField {
            key: "org.ozma/node.cpu_temp".to_string(),
            value_type: "real".to_string(),
            unit: Some("celsius".to_string()),
            enum_values: Vec::new(),
            required: true,
            default_value: Some("0".to_string()),
        }],
        update_mode: "delta".to_string(),
        max_update_rate_hz: Some(30),
        version: Some(1),
    }
}

fn sample_display_definition() -> DisplayDefinition {
    DisplayDefinition {
        id: "org.ozma.dashboard".to_string(),
        version: 4,
        hash: vec![0xaa, 0xbb, 0xcc],
        definition_json: br#"{"schema":1,"layout":{"type":"column"}}"#.to_vec(),
        definition_zstd: vec![0x28, 0xb5, 0x2f],
        fields: vec![
            FieldDef {
                id: 1,
                key: "org.ozma/node.cpu_temp".to_string(),
                r#type: ValueType::Real as i32,
                unit: Some("celsius".to_string()),
                scale: Some(100),
                description: Some("CPU temperature".to_string()),
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
        actions: vec![action_def()],
    }
}

fn action_def() -> ozma_protocol_proto::v1::ActionDef {
    ozma_protocol_proto::v1::ActionDef {
        id: 7,
        key: "org.ozma.scenario.activate".to_string(),
        params_schema_json: br#"{"type":"object"}"#.to_vec(),
        permissions: vec!["scenario.control".to_string()],
        requires_confirmation: true,
        label: Some("Activate".to_string()),
        description: Some("Activate a scenario".to_string()),
    }
}

fn sample_data_update() -> DataUpdate {
    DataUpdate {
        definition_version: 4,
        values: vec![
            FieldValue {
                field_id: 1,
                value: Some(field_value::Value::Real(42.5)),
            },
            FieldValue {
                field_id: 2,
                value: Some(field_value::Value::Unsigned(1200)),
            },
            FieldValue {
                field_id: 3,
                value: Some(field_value::Value::Integer(-9)),
            },
            FieldValue {
                field_id: 4,
                value: Some(field_value::Value::Text("ready".to_string())),
            },
        ],
        sequence: Some(99),
        timestamp_micros: Some(1_700_000_000_000_000),
    }
}

fn sample_scene_commit() -> SceneCommit {
    SceneCommit {
        revision: 12,
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
                        definition_version: 4,
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
                operation: Some(scene_op::Operation::Move(NodeMove {
                    parent: Some(1),
                    index: Some(0),
                })),
            },
            SceneOp {
                node_id: 4,
                operation: Some(scene_op::Operation::Remove(true)),
            },
        ],
        base_revision: Some(11),
        definition_id: Some("org.ozma.dashboard".to_string()),
    }
}

fn sample_action_invoke() -> ozma_protocol_proto::v1::ActionInvoke {
    ozma_protocol_proto::v1::ActionInvoke {
        action_id: 7,
        params: vec![0x08, 0x01],
        idempotency_key: 0x0123_4567_89ab_cdef,
        params_json: Some(br#"{"scenario":"focus"}"#.to_vec()),
        timestamp_micros: Some(1_700_000_000_000_001),
    }
}

#[test]
fn hello_roundtrip() {
    roundtrip(&sample_hello());
}

#[test]
fn display_definition_roundtrip() {
    roundtrip(&sample_display_definition());
}

#[test]
fn data_schema_roundtrip() {
    roundtrip(&sample_data_schema());
}

#[test]
fn hello_ack_carries_definition_and_wire_format() {
    let ack = HelloAck {
        session: Some(Session {
            id: "sess-01".to_string(),
            node_id: "node-01".to_string(),
            created_at_micros: 1_700_000_000_000_000,
            ..Default::default()
        }),
        wire_format: WireFormat::Protobuf as i32,
        display: Some(DisplayCapabilities {
            scene_versions: vec![1],
            ..Default::default()
        }),
        protocol: Some(ProtocolVersion { major: 1, minor: 0 }),
        server_id: Some("controller-01".to_string()),
        definition: Some(DefinitionRef {
            id: "org.ozma.dashboard".to_string(),
            version: 4,
            hash: vec![1, 2, 3],
        }),
        need_definition: true,
    };
    roundtrip(&ack);
}

#[test]
fn api_error_uses_a_stable_string_code() {
    let error = ApiError {
        code: "display.unsupported".to_string(),
        message: "no matching display method".to_string(),
        detail: Some("device does not advertise typed data".to_string()),
        details_json: Some(br#"{"device":"panel-1"}"#.to_vec()),
        retryable: false,
        retry_after_micros: None,
    };
    roundtrip(&error);
    assert_eq!(error.code, "display.unsupported");
}

#[test]
fn definition_ref_roundtrip() {
    roundtrip(&DefinitionRef {
        id: "org.ozma.dashboard".to_string(),
        version: 4,
        hash: vec![0xaa, 0xbb, 0xcc],
    });
}

#[test]
fn data_update_roundtrip() {
    roundtrip(&sample_data_update());
}

#[test]
fn scene_commit_roundtrip() {
    roundtrip(&sample_scene_commit());
}

#[test]
fn action_invoke_roundtrip() {
    roundtrip(&sample_action_invoke());
}

#[test]
fn scene_snapshot_roundtrip() {
    let snapshot = SceneSnapshot {
        revision: 12,
        nodes: vec![SceneNode {
            id: 1,
            parent: None,
            transform: Some(Transform::default()),
            clip: None,
            z: 0,
            content: Some(scene_node::Content::Video(VideoSurface {
                id: 1,
                channel: 3,
                width: Some(1920),
                height: Some(1080),
                codec: Some("h264".to_string()),
                track_id: Some("main".to_string()),
            })),
            interaction: None,
            visible: Some(true),
            name: None,
        }],
        roots: vec![1],
        base_revision: Some(11),
        definition_id: Some("org.ozma.dashboard".to_string()),
    };
    roundtrip(&snapshot);
}

#[test]
fn definition_patch_roundtrip() {
    let patch = DefinitionPatch {
        definition_id: "org.ozma.dashboard".to_string(),
        base_version: 4,
        version: 5,
        patch_json: br#"[{"op":"replace","path":"/title","value":"New"}]"#.to_vec(),
        hash: vec![1, 2, 3],
        patch_zstd: Some(vec![0x28, 0xb5, 0x2f]),
    };
    roundtrip(&patch);
}

#[test]
fn json_roundtrip_uses_pbjson() {
    let hello = sample_hello();
    let json = serde_json::to_string(&hello).expect("serialize Hello as proto3 JSON");
    let decoded: Hello = serde_json::from_str(&json).expect("deserialize Hello from proto3 JSON");
    assert_eq!(hello, decoded);
}

#[test]
fn descriptor_set_is_embedded() {
    assert!(
        !ozma_protocol_proto::FILE_DESCRIPTOR_SET.is_empty(),
        "build.rs must emit a descriptor set"
    );
}
