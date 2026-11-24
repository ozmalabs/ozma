// SPDX-License-Identifier: AGPL-3.0-only
//! Conversions between the shared domain model and generated protobuf types.
//!
//! The domain types live in [`ozma_protocol`]; the wire types are generated into
//! [`crate::proto`]. Neither crate knows about the other, so these conversions
//! are free functions rather than `From`/`TryFrom` implementations (the orphan
//! rules would reject a trait implementation for two foreign types).
//!
//! Conversions are fallible where the protobuf representation can express
//! values the domain model cannot:
//!
//! - the protobuf enums have an `UNSPECIFIED` zero value and may carry unknown
//!   numeric values;
//! - field value types are strings in the domain model and a typed enum on the
//!   wire;
//! - action parameter schemas are strings in the domain model and `bytes` on
//!   the wire.
//!
//! Nothing in this module panics on malformed input. Fields that exist only on
//! one side are documented on the individual conversion functions.

use ozma_protocol::display_method::{
    ActionDef, DataField, DataSchema, DisplayMethod, FieldDef, Interaction, InteractionTarget,
    MethodLocation, RenderingTier,
};
use ozma_protocol::scene::DisplayCapabilities;

use crate::proto;

/// Errors returned when a protobuf value cannot be represented in the domain
/// model, or a domain value cannot be represented on the wire.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConvertError {
    /// A protobuf enum carried a numeric value with no domain equivalent.
    #[error("unknown {kind} enum value {value}")]
    UnknownEnumValue {
        /// Enum name, for example `RenderingTier`.
        kind: &'static str,
        /// Raw protobuf numeric value.
        value: i32,
    },

    /// A protobuf enum was left at its `UNSPECIFIED` zero value, which has no
    /// domain equivalent.
    #[error("{kind} enum is unspecified")]
    UnspecifiedEnum {
        /// Enum name, for example `MethodLocation`.
        kind: &'static str,
    },

    /// A field `value_type` string is not one of the eight supported types.
    #[error("unknown field value type `{0}`")]
    UnknownValueType(String),

    /// A `bytes` field that carries UTF-8 text was not valid UTF-8.
    #[error("invalid UTF-8 in {context}: {source}")]
    MalformedUtf8 {
        /// Human-readable description of the field being decoded.
        context: &'static str,
        /// The underlying UTF-8 validation error.
        #[source]
        source: std::str::Utf8Error,
    },

    /// A scene version does not fit in the domain model's `u16`.
    #[error("scene version {value} does not fit in a u16")]
    SceneVersionOutOfRange {
        /// The out-of-range protobuf value.
        value: u32,
    },
}

/// Converts a domain rendering tier to its protobuf enum.
pub fn rendering_tier_to_proto(value: RenderingTier) -> proto::RenderingTier {
    match value {
        RenderingTier::PixelSink => proto::RenderingTier::PixelSink,
        RenderingTier::RenderedUi => proto::RenderingTier::RenderedUi,
        RenderingTier::NativeWidgets => proto::RenderingTier::NativeWidgets,
        RenderingTier::DataDriven => proto::RenderingTier::DataDriven,
    }
}

/// Converts a non-`UNSPECIFIED` protobuf rendering tier to the domain model.
pub fn rendering_tier_from_proto(
    value: proto::RenderingTier,
) -> Result<RenderingTier, ConvertError> {
    match value {
        proto::RenderingTier::Unspecified => Err(ConvertError::UnspecifiedEnum {
            kind: "RenderingTier",
        }),
        proto::RenderingTier::PixelSink => Ok(RenderingTier::PixelSink),
        proto::RenderingTier::RenderedUi => Ok(RenderingTier::RenderedUi),
        proto::RenderingTier::NativeWidgets => Ok(RenderingTier::NativeWidgets),
        proto::RenderingTier::DataDriven => Ok(RenderingTier::DataDriven),
    }
}

/// Decodes a raw protobuf rendering-tier value.
pub fn rendering_tier_from_i32(value: i32) -> Result<RenderingTier, ConvertError> {
    let value =
        proto::RenderingTier::try_from(value).map_err(|error| ConvertError::UnknownEnumValue {
            kind: "RenderingTier",
            value: error.0,
        })?;
    rendering_tier_from_proto(value)
}

/// Converts a domain method location to its protobuf enum.
pub fn method_location_to_proto(value: MethodLocation) -> proto::MethodLocation {
    match value {
        MethodLocation::Device => proto::MethodLocation::Device,
        MethodLocation::GraphService => proto::MethodLocation::GraphService,
        MethodLocation::Either => proto::MethodLocation::Either,
    }
}

/// Converts a non-`UNSPECIFIED` protobuf method location to the domain model.
pub fn method_location_from_proto(
    value: proto::MethodLocation,
) -> Result<MethodLocation, ConvertError> {
    match value {
        proto::MethodLocation::Unspecified => Err(ConvertError::UnspecifiedEnum {
            kind: "MethodLocation",
        }),
        proto::MethodLocation::Device => Ok(MethodLocation::Device),
        proto::MethodLocation::GraphService => Ok(MethodLocation::GraphService),
        proto::MethodLocation::Either => Ok(MethodLocation::Either),
    }
}

/// Decodes a raw protobuf method-location value.
pub fn method_location_from_i32(value: i32) -> Result<MethodLocation, ConvertError> {
    let value =
        proto::MethodLocation::try_from(value).map_err(|error| ConvertError::UnknownEnumValue {
            kind: "MethodLocation",
            value: error.0,
        })?;
    method_location_from_proto(value)
}

/// Converts a domain interaction modality to its protobuf enum.
pub fn interaction_to_proto(value: Interaction) -> proto::Interaction {
    match value {
        Interaction::Pointer => proto::Interaction::Pointer,
        Interaction::Touch => proto::Interaction::Touch,
        Interaction::Keyboard => proto::Interaction::Keyboard,
        Interaction::Buttons => proto::Interaction::Buttons,
        Interaction::Gamepad => proto::Interaction::Gamepad,
        Interaction::Pen => proto::Interaction::Pen,
        Interaction::Focus => proto::Interaction::Focus,
    }
}

/// Converts a non-`UNSPECIFIED` protobuf interaction to the domain model.
pub fn interaction_from_proto(value: proto::Interaction) -> Result<Interaction, ConvertError> {
    match value {
        proto::Interaction::Unspecified => Err(ConvertError::UnspecifiedEnum {
            kind: "Interaction",
        }),
        proto::Interaction::Pointer => Ok(Interaction::Pointer),
        proto::Interaction::Touch => Ok(Interaction::Touch),
        proto::Interaction::Keyboard => Ok(Interaction::Keyboard),
        proto::Interaction::Buttons => Ok(Interaction::Buttons),
        proto::Interaction::Gamepad => Ok(Interaction::Gamepad),
        proto::Interaction::Pen => Ok(Interaction::Pen),
        proto::Interaction::Focus => Ok(Interaction::Focus),
    }
}

/// Decodes a raw protobuf interaction value.
pub fn interaction_from_i32(value: i32) -> Result<Interaction, ConvertError> {
    let value =
        proto::Interaction::try_from(value).map_err(|error| ConvertError::UnknownEnumValue {
            kind: "Interaction",
            value: error.0,
        })?;
    interaction_from_proto(value)
}

/// Converts an interaction list to protobuf enum values.
pub fn interactions_to_proto(values: &[Interaction]) -> Vec<i32> {
    values
        .iter()
        .copied()
        .map(|value| interaction_to_proto(value) as i32)
        .collect()
}

/// Converts protobuf interaction enum values to the domain model.
pub fn interactions_from_proto(values: &[i32]) -> Result<Vec<Interaction>, ConvertError> {
    values.iter().copied().map(interaction_from_i32).collect()
}

/// Converts a domain display method to its protobuf representation.
pub fn display_method_to_proto(method: &DisplayMethod) -> proto::DisplayMethod {
    proto::DisplayMethod {
        id: method.id.clone(),
        accepts: method.accepts.clone(),
        schemas: method.schemas.clone(),
        location: method_location_to_proto(method.location) as i32,
        outputs: method.outputs.clone(),
        interactions: interactions_to_proto(&method.interactions),
        configuration_schema: method.configuration_schema.clone(),
    }
}

/// Converts a protobuf display method to the domain model.
pub fn display_method_from_proto(
    method: &proto::DisplayMethod,
) -> Result<DisplayMethod, ConvertError> {
    Ok(DisplayMethod {
        id: method.id.clone(),
        accepts: method.accepts.clone(),
        schemas: method.schemas.clone(),
        location: method_location_from_i32(method.location)?,
        outputs: method.outputs.clone(),
        interactions: interactions_from_proto(&method.interactions)?,
        configuration_schema: method.configuration_schema.clone(),
    })
}

/// Converts a domain data field to its protobuf representation.
///
/// The domain `default` attribute maps to `default_value` on the wire.
pub fn data_field_to_proto(field: &DataField) -> proto::DataField {
    proto::DataField {
        key: field.key.clone(),
        value_type: field.value_type.clone(),
        unit: field.unit.clone(),
        enum_values: field.enum_values.clone(),
        required: field.required,
        default_value: field.default.clone(),
    }
}

/// Converts a protobuf data field to the domain model.
pub fn data_field_from_proto(field: &proto::DataField) -> DataField {
    DataField {
        key: field.key.clone(),
        value_type: field.value_type.clone(),
        unit: field.unit.clone(),
        enum_values: field.enum_values.clone(),
        required: field.required,
        default: field.default_value.clone(),
    }
}

/// Converts a domain data schema to its protobuf representation.
///
/// The protobuf `version` field has no domain counterpart and is left unset.
pub fn data_schema_to_proto(schema: &DataSchema) -> proto::DataSchema {
    proto::DataSchema {
        id: schema.id.clone(),
        fields: schema.fields.iter().map(data_field_to_proto).collect(),
        update_mode: schema.update_mode.clone(),
        max_update_rate_hz: schema.max_update_rate_hz,
        version: None,
    }
}

/// Converts a protobuf data schema to the domain model.
///
/// The protobuf `version` field is discarded because the domain schema does
/// not carry one (definition versions live in the display definition).
pub fn data_schema_from_proto(schema: &proto::DataSchema) -> DataSchema {
    DataSchema {
        id: schema.id.clone(),
        fields: schema.fields.iter().map(data_field_from_proto).collect(),
        update_mode: schema.update_mode.clone(),
        max_update_rate_hz: schema.max_update_rate_hz,
    }
}

/// Converts domain display capabilities to their protobuf representation.
pub fn display_capabilities_to_proto(
    capabilities: &DisplayCapabilities,
) -> proto::DisplayCapabilities {
    proto::DisplayCapabilities {
        scene_versions: capabilities
            .scene_versions
            .iter()
            .map(|version| u32::from(*version))
            .collect(),
        methods: capabilities
            .methods
            .iter()
            .map(display_method_to_proto)
            .collect(),
        tiers: capabilities
            .tiers
            .iter()
            .copied()
            .map(|tier| rendering_tier_to_proto(tier) as i32)
            .collect(),
        schemas: capabilities.schemas.clone(),
        max_update_rate_hz: capabilities.max_update_rate_hz,
        resource_budget_bytes: capabilities.resource_budget_bytes,
    }
}

/// Converts protobuf display capabilities to the domain model.
///
/// Protobuf scene versions are `uint32`; values that do not fit in the domain
/// model's `u16` are rejected instead of being truncated.
pub fn display_capabilities_from_proto(
    capabilities: &proto::DisplayCapabilities,
) -> Result<DisplayCapabilities, ConvertError> {
    let scene_versions = capabilities
        .scene_versions
        .iter()
        .map(|value| {
            u16::try_from(*value)
                .map_err(|_| ConvertError::SceneVersionOutOfRange { value: *value })
        })
        .collect::<Result<Vec<_>, _>>()?;

    let tiers = capabilities
        .tiers
        .iter()
        .copied()
        .map(rendering_tier_from_i32)
        .collect::<Result<Vec<_>, _>>()?;

    let methods = capabilities
        .methods
        .iter()
        .map(display_method_from_proto)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(DisplayCapabilities {
        scene_versions,
        methods,
        tiers,
        schemas: capabilities.schemas.clone(),
        max_update_rate_hz: capabilities.max_update_rate_hz,
        resource_budget_bytes: capabilities.resource_budget_bytes,
    })
}

/// Converts a compiled field definition to its protobuf representation.
///
/// The string `value_type` is validated and mapped to the protobuf
/// [`proto::ValueType`] enum. The protobuf `description` field has no domain
/// counterpart and is left unset.
pub fn field_def_to_proto(field: &FieldDef) -> Result<proto::FieldDef, ConvertError> {
    let value_type = ozma_display_definition::FieldValueType::from_name(&field.value_type)
        .ok_or_else(|| ConvertError::UnknownValueType(field.value_type.clone()))?;

    Ok(proto::FieldDef {
        id: field.id,
        key: field.key.clone(),
        r#type: value_type.to_proto() as i32,
        unit: field.unit.clone(),
        scale: field.scale,
        description: None,
    })
}

/// Converts a protobuf compiled field definition to the domain model.
///
/// The resulting `value_type` is the canonical lower-case name. The protobuf
/// `description` field is discarded.
pub fn field_def_from_proto(field: &proto::FieldDef) -> Result<FieldDef, ConvertError> {
    let value_type = proto::ValueType::try_from(field.r#type).map_err(|error| {
        ConvertError::UnknownEnumValue {
            kind: "ValueType",
            value: error.0,
        }
    })?;

    let value_type = match value_type {
        proto::ValueType::Unspecified => {
            return Err(ConvertError::UnspecifiedEnum { kind: "ValueType" })
        }
        proto::ValueType::Sint => "sint",
        proto::ValueType::Uint => "uint",
        proto::ValueType::Real => "real",
        proto::ValueType::Bool => "bool",
        proto::ValueType::String => "string",
        proto::ValueType::Enum => "enum",
        proto::ValueType::Bytes => "bytes",
        proto::ValueType::Timestamp => "timestamp",
    };

    Ok(FieldDef {
        id: field.id,
        key: field.key.clone(),
        value_type: value_type.to_owned(),
        unit: field.unit.clone(),
        scale: field.scale,
    })
}

/// Converts a domain action definition to its protobuf representation.
///
/// `params_schema` is encoded as UTF-8 bytes; `None` becomes empty bytes. The
/// protobuf `label` and `description` fields have no domain counterpart and
/// are left unset.
pub fn action_def_to_proto(action: &ActionDef) -> proto::ActionDef {
    proto::ActionDef {
        id: action.id,
        key: action.key.clone(),
        params_schema_json: action
            .params_schema
            .as_deref()
            .map(|schema| schema.as_bytes().to_vec())
            .unwrap_or_default(),
        permissions: action.permissions.clone(),
        requires_confirmation: action.requires_confirmation,
        label: None,
        description: None,
    }
}

/// Converts a protobuf action definition to the domain model.
///
/// Empty `params_schema_json` becomes `None`, matching [`action_def_to_proto`].
/// Invalid UTF-8 is rejected rather than lossily replaced.
pub fn action_def_from_proto(action: &proto::ActionDef) -> Result<ActionDef, ConvertError> {
    let params_schema = if action.params_schema_json.is_empty() {
        None
    } else {
        Some(
            std::str::from_utf8(&action.params_schema_json)
                .map_err(|source| ConvertError::MalformedUtf8 {
                    context: "ActionDef.params_schema_json",
                    source,
                })?
                .to_owned(),
        )
    };

    Ok(ActionDef {
        id: action.id,
        key: action.key.clone(),
        params_schema,
        permissions: action.permissions.clone(),
        requires_confirmation: action.requires_confirmation,
    })
}

/// Converts a domain interaction target to its protobuf representation.
pub fn interaction_target_to_proto(target: &InteractionTarget) -> proto::InteractionTarget {
    proto::InteractionTarget {
        action_id: target.action_id,
        node_id: target.node_id,
        accessibility_label: target.accessibility_label.clone(),
        cursor: target.cursor.clone(),
    }
}

/// Converts a protobuf interaction target to the domain model.
pub fn interaction_target_from_proto(target: &proto::InteractionTarget) -> InteractionTarget {
    InteractionTarget {
        action_id: target.action_id,
        node_id: target.node_id,
        accessibility_label: target.accessibility_label.clone(),
        cursor: target.cursor.clone(),
    }
}

// `ContentRequirement` has no protobuf message in `ozma.v1`; it is a
// negotiation-time domain query rather than a wire artifact, so no conversion
// is provided here.
