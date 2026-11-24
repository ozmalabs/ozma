// SPDX-License-Identifier: AGPL-3.0-only
//! Round-trip and error tests for domain <-> protobuf conversions.

use ozma_display_protocol::convert::{
    action_def_from_proto, action_def_to_proto, data_field_from_proto, data_field_to_proto,
    data_schema_from_proto, data_schema_to_proto, display_capabilities_from_proto,
    display_capabilities_to_proto, display_method_from_proto, display_method_to_proto,
    field_def_from_proto, field_def_to_proto, interaction_from_i32, interaction_from_proto,
    interaction_target_from_proto, interaction_target_to_proto, interaction_to_proto,
    interactions_from_proto, interactions_to_proto, method_location_from_i32,
    method_location_from_proto, method_location_to_proto, rendering_tier_from_i32,
    rendering_tier_from_proto, rendering_tier_to_proto,
};
use ozma_display_protocol::{proto, ConvertError};
use ozma_protocol::display_method::{
    ActionDef, DataField, DataSchema, DisplayMethod, FieldDef, Interaction, InteractionTarget,
    MethodLocation, RenderingTier,
};
use ozma_protocol::scene::DisplayCapabilities;

const ALL_TIERS: [RenderingTier; 4] = [
    RenderingTier::PixelSink,
    RenderingTier::RenderedUi,
    RenderingTier::NativeWidgets,
    RenderingTier::DataDriven,
];

const ALL_LOCATIONS: [MethodLocation; 3] = [
    MethodLocation::Device,
    MethodLocation::GraphService,
    MethodLocation::Either,
];

const ALL_INTERACTIONS: [Interaction; 7] = [
    Interaction::Pointer,
    Interaction::Touch,
    Interaction::Keyboard,
    Interaction::Buttons,
    Interaction::Gamepad,
    Interaction::Pen,
    Interaction::Focus,
];

fn sample_method() -> DisplayMethod {
    DisplayMethod {
        id: "org.ozma.test/gauge".to_string(),
        accepts: vec!["application/vnd.ozma.scene+json".to_string()],
        schemas: vec!["org.ozma.test/schema".to_string()],
        location: MethodLocation::Either,
        outputs: vec!["image/png".to_string()],
        interactions: ALL_INTERACTIONS.to_vec(),
        configuration_schema: Some(r#"{"type":"object"}"#.to_string()),
    }
}

fn sample_schema() -> DataSchema {
    DataSchema {
        id: "org.ozma.test/schema".to_string(),
        fields: vec![
            DataField {
                key: "org.ozma/test.temp".to_string(),
                value_type: "sint".to_string(),
                unit: Some("celsius".to_string()),
                enum_values: Vec::new(),
                required: true,
                default: Some("0".to_string()),
            },
            DataField {
                key: "org.ozma/test.mode".to_string(),
                value_type: "enum".to_string(),
                unit: None,
                enum_values: vec!["off".to_string(), "on".to_string()],
                required: false,
                default: None,
            },
        ],
        update_mode: "push".to_string(),
        max_update_rate_hz: Some(30),
    }
}

#[test]
fn rendering_tiers_round_trip() {
    for tier in ALL_TIERS {
        let proto_value = rendering_tier_to_proto(tier);
        assert_eq!(rendering_tier_from_proto(proto_value), Ok(tier));
        assert_eq!(rendering_tier_from_i32(proto_value as i32), Ok(tier));
    }
}

#[test]
fn method_locations_round_trip() {
    for location in ALL_LOCATIONS {
        let proto_value = method_location_to_proto(location);
        assert_eq!(method_location_from_proto(proto_value), Ok(location));
        assert_eq!(method_location_from_i32(proto_value as i32), Ok(location));
    }
}

#[test]
fn interactions_round_trip() {
    for interaction in ALL_INTERACTIONS {
        let proto_value = interaction_to_proto(interaction);
        assert_eq!(interaction_from_proto(proto_value), Ok(interaction));
        assert_eq!(interaction_from_i32(proto_value as i32), Ok(interaction));
    }

    assert_eq!(
        interactions_from_proto(&interactions_to_proto(&ALL_INTERACTIONS)),
        Ok(ALL_INTERACTIONS.to_vec())
    );
}

#[test]
fn display_method_round_trips() {
    let method = sample_method();
    let proto_method = display_method_to_proto(&method);
    assert_eq!(display_method_from_proto(&proto_method), Ok(method));
}

#[test]
fn data_field_and_schema_round_trip() {
    let schema = sample_schema();
    let proto_schema = data_schema_to_proto(&schema);
    assert_eq!(data_schema_from_proto(&proto_schema), schema);

    let field = &schema.fields[0];
    let proto_field = data_field_to_proto(field);
    assert_eq!(&data_field_from_proto(&proto_field), field);
}

#[test]
fn display_capabilities_round_trip() {
    let capabilities = DisplayCapabilities {
        scene_versions: vec![1, 2],
        methods: vec![sample_method()],
        tiers: ALL_TIERS.to_vec(),
        schemas: vec!["org.ozma.test/schema".to_string()],
        max_update_rate_hz: Some(60),
        resource_budget_bytes: 32 * 1024 * 1024,
    };

    let proto_capabilities = display_capabilities_to_proto(&capabilities);
    assert_eq!(
        display_capabilities_from_proto(&proto_capabilities),
        Ok(capabilities)
    );
}

#[test]
fn field_def_round_trips_through_the_value_type_enum() {
    let field = FieldDef {
        id: 3,
        key: "org.ozma/test.temp".to_string(),
        value_type: "sint".to_string(),
        unit: Some("celsius".to_string()),
        scale: Some(100),
    };

    let proto_field = field_def_to_proto(&field).expect("known value type");
    assert_eq!(proto_field.r#type, proto::ValueType::Sint as i32);
    assert_eq!(field_def_from_proto(&proto_field), Ok(field));
}

#[test]
fn action_def_round_trips_params_schema() {
    let action = ActionDef {
        id: 7,
        key: "org.ozma.scenario.activate".to_string(),
        params_schema: Some(r#"{"type":"object"}"#.to_string()),
        permissions: vec!["scenario.control".to_string()],
        requires_confirmation: true,
    };

    let proto_action = action_def_to_proto(&action);
    assert_eq!(action_def_from_proto(&proto_action), Ok(action.clone()));

    let no_schema = ActionDef {
        params_schema: None,
        ..action
    };
    assert_eq!(
        action_def_from_proto(&action_def_to_proto(&no_schema)),
        Ok(no_schema)
    );
}

#[test]
fn interaction_target_round_trips() {
    let target = InteractionTarget {
        action_id: Some(7),
        node_id: Some(42),
        accessibility_label: Some("CPU temperature".to_string()),
        cursor: Some("pointer".to_string()),
    };

    let proto_target = interaction_target_to_proto(&target);
    assert_eq!(interaction_target_from_proto(&proto_target), target);
}

#[test]
fn unknown_enum_values_are_rejected() {
    assert!(matches!(
        rendering_tier_from_i32(99),
        Err(ConvertError::UnknownEnumValue {
            kind: "RenderingTier",
            value: 99
        })
    ));
    assert!(matches!(
        method_location_from_i32(99),
        Err(ConvertError::UnknownEnumValue {
            kind: "MethodLocation",
            value: 99
        })
    ));
    assert!(matches!(
        interaction_from_i32(99),
        Err(ConvertError::UnknownEnumValue {
            kind: "Interaction",
            value: 99
        })
    ));

    let mut proto_field = proto::FieldDef {
        id: 1,
        key: "org.ozma/test.temp".to_string(),
        r#type: proto::ValueType::Sint as i32,
        unit: None,
        scale: None,
        description: None,
    };
    proto_field.r#type = 99;
    assert!(matches!(
        field_def_from_proto(&proto_field),
        Err(ConvertError::UnknownEnumValue {
            kind: "ValueType",
            value: 99
        })
    ));
}

#[test]
fn unspecified_enum_values_are_rejected() {
    assert!(matches!(
        rendering_tier_from_i32(0),
        Err(ConvertError::UnspecifiedEnum {
            kind: "RenderingTier"
        })
    ));
    assert!(matches!(
        method_location_from_i32(0),
        Err(ConvertError::UnspecifiedEnum {
            kind: "MethodLocation"
        })
    ));
    assert!(matches!(
        interaction_from_i32(0),
        Err(ConvertError::UnspecifiedEnum {
            kind: "Interaction"
        })
    ));
}

#[test]
fn unknown_domain_value_types_are_rejected() {
    let field = FieldDef {
        id: 1,
        key: "org.ozma/test.mystery".to_string(),
        value_type: "quantum".to_string(),
        unit: None,
        scale: None,
    };

    assert!(matches!(
        field_def_to_proto(&field),
        Err(ConvertError::UnknownValueType(value)) if value == "quantum"
    ));
}

#[test]
fn malformed_utf8_in_action_schema_is_rejected() {
    let action = ActionDef {
        id: 7,
        key: "org.ozma.scenario.activate".to_string(),
        params_schema: Some(r#"{"type":"object"}"#.to_string()),
        permissions: Vec::new(),
        requires_confirmation: false,
    };
    let mut proto_action = action_def_to_proto(&action);
    proto_action.params_schema_json = vec![0xff, 0xfe];

    assert!(matches!(
        action_def_from_proto(&proto_action),
        Err(ConvertError::MalformedUtf8 {
            context: "ActionDef.params_schema_json",
            ..
        })
    ));
}

#[test]
fn out_of_range_scene_version_is_rejected() {
    let mut proto_capabilities = proto::DisplayCapabilities {
        scene_versions: vec![u32::from(u16::MAX) + 1],
        ..Default::default()
    };
    assert!(matches!(
        display_capabilities_from_proto(&proto_capabilities),
        Err(ConvertError::SceneVersionOutOfRange { value })
            if value == u32::from(u16::MAX) + 1
    ));

    proto_capabilities.scene_versions = vec![1];
    assert!(display_capabilities_from_proto(&proto_capabilities).is_ok());
}
