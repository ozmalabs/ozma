// SPDX-License-Identifier: AGPL-3.0-only
//! Canonical display-definition documents.
//!
//! A display definition is authored as JSON. This crate owns the rules that
//! make that JSON safe to use as a protocol artifact:
//!
//! - parsing and semantic validation of the document, including the compiled
//!   field and action tables;
//! - deterministic canonical JSON (sorted object keys, compact output);
//! - a BLAKE3 hash over the canonical bytes;
//! - RFC 6902 JSON Patch application with re-validation; and
//! - conversion to the `ozma.v1.DisplayDefinition` protobuf message.
//!
//! The document is deliberately self-contained: it depends only on the shared
//! protocol model and the generated protobuf types, not on display, session, or
//! transport crates.
//!
//! # Document shape
//!
//! ```json
//! {
//!   "id": "org.ozma.dashboard/status",
//!   "version": 3,
//!   "fields": [
//!     {
//!       "id": 1,
//!       "key": "org.ozma/node.cpu_temp",
//!       "type": "sint",
//!       "unit": "celsius",
//!       "scale": 100
//!     }
//!   ],
//!   "actions": [
//!     {
//!       "id": 1,
//!       "key": "org.ozma.scenario.activate",
//!       "params_schema": "{\"type\":\"object\"}",
//!       "permissions": ["scenario.control"]
//!     }
//!   ],
//!   "layout": { "type": "column", "children": [] }
//! }
//! ```
//!
//! Field and action identifiers are assigned once and never reused. Field
//! `type` strings are accepted case-insensitively and normalized in the
//! compiled table. Unknown types, duplicate identifiers, and empty keys are
//! rejected.
//!
//! # Canonical hashing
//!
//! [`DisplayDefinitionDocument::canonical_json`] recursively sorts object keys
//! and emits no insignificant whitespace. Array order is preserved. Integral
//! floating-point values that are exactly representable are emitted in integer
//! form, so `1` and `1.0` hash alike. String escaping and floating-point output
//! are deterministic (provided by `serde_json`).
//!
//! # Field values
//!
//! [`DisplayDefinitionDocument::encode_field_value`] and
//! [`DisplayDefinitionDocument::decode_field_value`] operate on the *wire*
//! representation used by `ozma.v1.FieldValue`. For `sint` and `uint` fields
//! with a declared `scale`, integer JSON inputs are already-scaled integers and
//! are preserved exactly; non-integral JSON numbers are multiplied by the
//! scale and must land on an exactly representable integer. A `real` field is
//! stored as a double; its `scale` is metadata only. `timestamp` values are
//! integer microseconds, and `enum` values are integer enum numbers because a
//! definition does not carry an enum vocabulary.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;

use ozma_protocol_proto::v1::{self, field_value};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Number, Value};
use thiserror::Error;

pub use ozma_protocol::display_method::{ActionDef, FieldDef};

/// Maximum accepted size of a display-definition document.
///
/// Definitions are sent once and then patched, so they are intentionally small
/// enough to be cached in memory by every adapter.
pub const MAX_DEFINITION_BYTES: usize = 1024 * 1024;

/// Maximum accepted size of an RFC 6902 JSON Patch document.
pub const MAX_PATCH_BYTES: usize = 1024 * 1024;

/// Largest integer that an `f64` can represent without losing precision.
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_992.0;

/// Errors returned while parsing, patching, or compiling a definition.
#[derive(Debug, Error)]
pub enum DefinitionError {
    /// The definition document exceeds [`MAX_DEFINITION_BYTES`].
    #[error("definition document is {size} bytes, exceeding the {limit}-byte limit")]
    DocumentTooLarge { size: usize, limit: usize },

    /// The patch document exceeds [`MAX_PATCH_BYTES`].
    #[error("definition patch is {size} bytes, exceeding the {limit}-byte limit")]
    PatchTooLarge { size: usize, limit: usize },

    /// The input is not valid JSON.
    #[error("invalid JSON: {0}")]
    InvalidJson(#[from] serde_json::Error),

    /// The JSON Patch could not be applied.
    #[error("invalid definition patch: {0}")]
    InvalidPatch(String),

    /// The root value is not a JSON object.
    #[error("display definition document must be a JSON object")]
    NotAnObject,

    /// A required attribute is absent.
    #[error("missing required document attribute `{0}`")]
    MissingAttribute(&'static str),

    /// An attribute has the wrong JSON type or is out of range.
    #[error("document attribute `{0}` must be {1}")]
    InvalidAttribute(&'static str, &'static str),

    /// A required string is empty or whitespace-only.
    #[error("`{0}` must not be empty")]
    EmptyAttribute(&'static str),

    /// Two fields use the same stable identifier.
    #[error("duplicate field id {0}")]
    DuplicateFieldId(u32),

    /// Two actions use the same stable identifier.
    #[error("duplicate action id {0}")]
    DuplicateActionId(u32),

    /// A field declares a type this crate does not understand.
    #[error("unknown field value type `{0}`")]
    UnknownValueType(String),

    /// No field with the requested identifier exists in this definition.
    #[error("unknown field id {0}")]
    UnknownFieldId(u32),

    /// A value's JSON or protobuf variant does not match the field's type.
    #[error("field {field_id} expects a {expected} value, got {actual}")]
    FieldTypeMismatch {
        field_id: u32,
        expected: &'static str,
        actual: &'static str,
    },

    /// A value is malformed or cannot be represented without losing data.
    #[error("field {field_id}: {message}")]
    InvalidFieldValue { field_id: u32, message: String },
}

/// The logical type of a definition field.
///
/// This is the typed view of the proto `ValueType` enum and of the document's
/// case-insensitive `type` string.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FieldValueType {
    /// Signed integer, optionally scaled.
    Sint,
    /// Unsigned integer, optionally scaled.
    Uint,
    /// IEEE-754 double.
    Real,
    /// Boolean flag.
    Bool,
    /// UTF-8 label or text.
    String,
    /// Integer enum number.
    Enum,
    /// Opaque bytes.
    Bytes,
    /// Timestamp in microseconds since the Unix epoch.
    Timestamp,
}

impl FieldValueType {
    /// Canonical lower-case name used in compiled field tables.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sint => "sint",
            Self::Uint => "uint",
            Self::Real => "real",
            Self::Bool => "bool",
            Self::String => "string",
            Self::Enum => "enum",
            Self::Bytes => "bytes",
            Self::Timestamp => "timestamp",
        }
    }

    /// Parse a field type name case-insensitively.
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "sint" => Some(Self::Sint),
            "uint" => Some(Self::Uint),
            "real" => Some(Self::Real),
            "bool" => Some(Self::Bool),
            "string" => Some(Self::String),
            "enum" => Some(Self::Enum),
            "bytes" => Some(Self::Bytes),
            "timestamp" => Some(Self::Timestamp),
            _ => None,
        }
    }

    /// Map a field type to the protobuf `ValueType` enum.
    pub fn to_proto(self) -> v1::ValueType {
        match self {
            Self::Sint => v1::ValueType::Sint,
            Self::Uint => v1::ValueType::Uint,
            Self::Real => v1::ValueType::Real,
            Self::Bool => v1::ValueType::Bool,
            Self::String => v1::ValueType::String,
            Self::Enum => v1::ValueType::Enum,
            Self::Bytes => v1::ValueType::Bytes,
            Self::Timestamp => v1::ValueType::Timestamp,
        }
    }
}

impl std::fmt::Display for FieldValueType {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::str::FromStr for FieldValueType {
    type Err = DefinitionError;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::from_name(name).ok_or_else(|| DefinitionError::UnknownValueType(name.to_owned()))
    }
}

impl Serialize for FieldValueType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for FieldValueType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let name = String::deserialize(deserializer)?;
        Self::from_name(&name)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown field value type `{name}`")))
    }
}

/// Resolve a compiled field's type string.
///
/// Fields returned by [`DisplayDefinitionDocument`] are always validated, but
/// this helper is available when a [`FieldDef`] is passed around independently.
pub fn field_value_type(field: &FieldDef) -> Result<FieldValueType, DefinitionError> {
    FieldValueType::from_name(&field.value_type)
        .ok_or_else(|| DefinitionError::UnknownValueType(field.value_type.clone()))
}

/// A validated, canonicalizable display definition.
///
/// The original parsed JSON is retained so that canonical hashing and patches
/// operate on the complete document, including layout and extension metadata
/// that this crate does not interpret.
#[derive(Clone, Debug, PartialEq)]
pub struct DisplayDefinitionDocument {
    id: String,
    version: u32,
    layout: Value,
    fields: Vec<FieldDef>,
    actions: Vec<ActionDef>,
    raw: Value,
}

impl DisplayDefinitionDocument {
    /// Parse and validate a definition from JSON bytes.
    ///
    /// The input must be a single JSON object and may not exceed
    /// [`MAX_DEFINITION_BYTES`]. Keys may appear in any order. The returned
    /// document keeps the parsed JSON for canonical hashing and patching.
    pub fn parse(bytes: &[u8]) -> Result<Self, DefinitionError> {
        if bytes.len() > MAX_DEFINITION_BYTES {
            return Err(DefinitionError::DocumentTooLarge {
                size: bytes.len(),
                limit: MAX_DEFINITION_BYTES,
            });
        }

        let raw = serde_json::from_slice(bytes)?;
        Self::from_parsed_value(raw)
    }

    /// Parse and validate a definition from a JSON string.
    pub fn parse_str(text: &str) -> Result<Self, DefinitionError> {
        Self::parse(text.as_bytes())
    }

    fn from_parsed_value(raw: Value) -> Result<Self, DefinitionError> {
        let object = raw.as_object().ok_or(DefinitionError::NotAnObject)?;

        let id = required_string(object, "id", "id")?;
        let version = required_u32(object, "version", "version")?;
        let layout = object.get("layout").cloned().unwrap_or(Value::Null);
        let fields = parse_fields(object.get("fields"))?;
        let actions = parse_actions(object.get("actions"))?;

        let document = Self {
            id,
            version,
            layout,
            fields,
            actions,
            raw,
        };

        let canonical_size = document.canonical_json()?.len();
        if canonical_size > MAX_DEFINITION_BYTES {
            return Err(DefinitionError::DocumentTooLarge {
                size: canonical_size,
                limit: MAX_DEFINITION_BYTES,
            });
        }

        Ok(document)
    }

    /// Stable definition identifier.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Definition version. Updates carry this value as `definition_version`.
    pub fn version(&self) -> u32 {
        self.version
    }

    /// The layout subtree exactly as authored.
    pub fn layout(&self) -> &Value {
        &self.layout
    }

    /// The compiled field table.
    pub fn fields(&self) -> &[FieldDef] {
        &self.fields
    }

    /// The compiled action table.
    pub fn actions(&self) -> &[ActionDef] {
        &self.actions
    }

    /// The complete parsed JSON document, including extension metadata.
    pub fn raw_document(&self) -> &Value {
        &self.raw
    }

    /// Look up a compiled field by its stable identifier.
    pub fn field(&self, id: u32) -> Option<&FieldDef> {
        self.fields.iter().find(|field| field.id == id)
    }

    /// Look up a compiled action by its stable identifier.
    pub fn action(&self, id: u32) -> Option<&ActionDef> {
        self.actions.iter().find(|action| action.id == id)
    }

    /// Encode the complete document as deterministic JSON.
    ///
    /// Object keys are sorted recursively, arrays keep their order, output is
    /// compact, and number/string encodings are stable.
    pub fn canonical_json(&self) -> Result<Vec<u8>, DefinitionError> {
        Ok(canonical_bytes(&self.raw))
    }

    /// BLAKE3 hash of [`Self::canonical_json`].
    pub fn hash(&self) -> [u8; 32] {
        *blake3::hash(&canonical_bytes(&self.raw)).as_bytes()
    }

    /// Apply an RFC 6902 JSON Patch and re-parse the resulting document.
    ///
    /// A failed patch leaves `self` untouched. The patched document is parsed
    /// and validated exactly like a full definition, including the size limit,
    /// field/action rules, and the canonical-size check.
    pub fn apply_patch(&self, patch_json: &[u8]) -> Result<Self, DefinitionError> {
        if patch_json.len() > MAX_PATCH_BYTES {
            return Err(DefinitionError::PatchTooLarge {
                size: patch_json.len(),
                limit: MAX_PATCH_BYTES,
            });
        }

        let patch: json_patch::Patch = serde_json::from_slice(patch_json)?;
        let mut patched = self.raw.clone();
        json_patch::patch(&mut patched, &patch.0)
            .map_err(|error| DefinitionError::InvalidPatch(error.to_string()))?;

        let patched_json = serde_json::to_vec(&patched)?;
        Self::parse(&patched_json)
    }

    /// Compile the document into its protobuf representation.
    pub fn to_proto(&self) -> Result<v1::DisplayDefinition, DefinitionError> {
        let fields = self
            .fields
            .iter()
            .map(field_to_proto)
            .collect::<Result<Vec<_>, _>>()?;
        let actions = self.actions.iter().map(action_to_proto).collect();

        Ok(v1::DisplayDefinition {
            id: self.id.clone(),
            version: self.version,
            hash: self.hash().to_vec(),
            definition_json: self.canonical_json()?,
            definition_zstd: Vec::new(),
            fields,
            actions,
        })
    }

    /// A cached reference for reconnect negotiation.
    pub fn definition_ref(&self) -> v1::DefinitionRef {
        v1::DefinitionRef {
            id: self.id.clone(),
            version: self.version,
            hash: self.hash().to_vec(),
        }
    }

    /// Encode a JSON value into the field's protobuf wire representation.
    ///
    /// For `sint`/`uint` fields with a `scale`, integer inputs are treated as
    /// already-scaled and preserved exactly; floating-point inputs are scaled
    /// and must be exactly representable. `real` values are stored directly.
    /// `timestamp` values are integer microseconds. `enum` values are integer
    /// numbers. `bytes` may be a string (UTF-8 bytes) or an array of 0-255
    /// integers.
    pub fn encode_field_value(
        &self,
        field_id: u32,
        value: &Value,
    ) -> Result<v1::FieldValue, DefinitionError> {
        let field = self
            .field(field_id)
            .ok_or(DefinitionError::UnknownFieldId(field_id))?;
        let value_type = field_value_type(field)?;

        let encoded = match value_type {
            FieldValueType::Sint => field_value::Value::Integer(encode_sint(field, value)?),
            FieldValueType::Uint => field_value::Value::Unsigned(encode_uint(field, value)?),
            FieldValueType::Real => field_value::Value::Real(encode_real(field, value)?),
            FieldValueType::Bool => field_value::Value::Flag(
                value
                    .as_bool()
                    .ok_or_else(|| mismatch(field_id, value_type, value))?,
            ),
            FieldValueType::String => field_value::Value::Text(
                value
                    .as_str()
                    .ok_or_else(|| mismatch(field_id, value_type, value))?
                    .to_owned(),
            ),
            FieldValueType::Enum => field_value::Value::EnumValue(encode_enum(field, value)?),
            FieldValueType::Bytes => field_value::Value::Blob(encode_bytes(field, value)?),
            FieldValueType::Timestamp => {
                field_value::Value::TimestampMicros(encode_timestamp(field, value)?)
            }
        };

        Ok(v1::FieldValue {
            field_id,
            value: Some(encoded),
        })
    }

    /// Decode a protobuf field value back into JSON.
    ///
    /// The value's oneof variant must match the field's declared type. For
    /// scaled integer fields the result is the wire-level scaled integer, which
    /// keeps the value exact. Byte blobs that are valid UTF-8 decode as strings;
    /// other blobs decode as arrays of byte numbers.
    pub fn decode_field_value(&self, value: &v1::FieldValue) -> Result<Value, DefinitionError> {
        let field = self
            .field(value.field_id)
            .ok_or(DefinitionError::UnknownFieldId(value.field_id))?;
        let expected = field_value_type(field)?;
        let Some(decoded) = &value.value else {
            return Err(DefinitionError::InvalidFieldValue {
                field_id: value.field_id,
                message: "field value has no value variant".to_owned(),
            });
        };

        let json = match expected {
            FieldValueType::Sint => match decoded {
                field_value::Value::Integer(integer) => number_from_i64(*integer),
                other => {
                    return Err(proto_mismatch(
                        value.field_id,
                        expected,
                        proto_variant_name(other),
                    ))
                }
            },
            FieldValueType::Uint => match decoded {
                field_value::Value::Unsigned(unsigned) => number_from_u64(*unsigned),
                other => {
                    return Err(proto_mismatch(
                        value.field_id,
                        expected,
                        proto_variant_name(other),
                    ))
                }
            },
            FieldValueType::Real => match decoded {
                field_value::Value::Real(real) => Number::from_f64(*real)
                    .map(Value::Number)
                    .ok_or_else(|| DefinitionError::InvalidFieldValue {
                        field_id: value.field_id,
                        message: "real value is not finite".to_owned(),
                    })?,
                other => {
                    return Err(proto_mismatch(
                        value.field_id,
                        expected,
                        proto_variant_name(other),
                    ))
                }
            },
            FieldValueType::Bool => match decoded {
                field_value::Value::Flag(flag) => Value::Bool(*flag),
                other => {
                    return Err(proto_mismatch(
                        value.field_id,
                        expected,
                        proto_variant_name(other),
                    ))
                }
            },
            FieldValueType::String => match decoded {
                field_value::Value::Text(text) => Value::String(text.clone()),
                other => {
                    return Err(proto_mismatch(
                        value.field_id,
                        expected,
                        proto_variant_name(other),
                    ))
                }
            },
            FieldValueType::Enum => match decoded {
                field_value::Value::EnumValue(enum_value) => {
                    number_from_i64(i64::from(*enum_value))
                }
                other => {
                    return Err(proto_mismatch(
                        value.field_id,
                        expected,
                        proto_variant_name(other),
                    ))
                }
            },
            FieldValueType::Bytes => match decoded {
                field_value::Value::Blob(blob) => decode_blob(blob),
                other => {
                    return Err(proto_mismatch(
                        value.field_id,
                        expected,
                        proto_variant_name(other),
                    ))
                }
            },
            FieldValueType::Timestamp => match decoded {
                field_value::Value::TimestampMicros(micros) => number_from_i64(*micros),
                other => {
                    return Err(proto_mismatch(
                        value.field_id,
                        expected,
                        proto_variant_name(other),
                    ))
                }
            },
        };

        Ok(json)
    }
}

fn parse_fields(value: Option<&Value>) -> Result<Vec<FieldDef>, DefinitionError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let array = value
        .as_array()
        .ok_or(DefinitionError::InvalidAttribute("fields", "an array"))?;

    let mut seen_ids = BTreeSet::new();
    let mut fields = Vec::with_capacity(array.len());

    for item in array {
        let object = item
            .as_object()
            .ok_or(DefinitionError::InvalidAttribute("fields[]", "an object"))?;

        let id = required_u32(object, "id", "fields[].id")?;
        if !seen_ids.insert(id) {
            return Err(DefinitionError::DuplicateFieldId(id));
        }

        let key = required_string(object, "key", "fields[].key")?;
        let type_value = object
            .get("type")
            .ok_or(DefinitionError::MissingAttribute("fields[].type"))?;
        let type_name = type_value
            .as_str()
            .ok_or(DefinitionError::InvalidAttribute(
                "fields[].type",
                "a string",
            ))?;
        let value_type = FieldValueType::from_name(type_name)
            .ok_or_else(|| DefinitionError::UnknownValueType(type_name.to_owned()))?;

        fields.push(FieldDef {
            id,
            key,
            value_type: value_type.as_str().to_owned(),
            unit: optional_string(object, "unit", "fields[].unit")?,
            scale: optional_i32(object, "scale", "fields[].scale")?,
        });
    }

    Ok(fields)
}

fn parse_actions(value: Option<&Value>) -> Result<Vec<ActionDef>, DefinitionError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let array = value
        .as_array()
        .ok_or(DefinitionError::InvalidAttribute("actions", "an array"))?;

    let mut seen_ids = BTreeSet::new();
    let mut actions = Vec::with_capacity(array.len());

    for item in array {
        let object = item
            .as_object()
            .ok_or(DefinitionError::InvalidAttribute("actions[]", "an object"))?;

        let id = required_u32(object, "id", "actions[].id")?;
        if !seen_ids.insert(id) {
            return Err(DefinitionError::DuplicateActionId(id));
        }

        actions.push(ActionDef {
            id,
            key: required_string(object, "key", "actions[].key")?,
            params_schema: optional_schema(object, "params_schema", "actions[].params_schema")?,
            permissions: optional_string_array(object, "permissions", "actions[].permissions")?,
            requires_confirmation: optional_bool(
                object,
                "requires_confirmation",
                "actions[].requires_confirmation",
            )?,
        });
    }

    Ok(actions)
}

fn required_string(
    object: &Map<String, Value>,
    key: &str,
    name: &'static str,
) -> Result<String, DefinitionError> {
    let value = object
        .get(key)
        .ok_or(DefinitionError::MissingAttribute(name))?;
    let text = value
        .as_str()
        .ok_or(DefinitionError::InvalidAttribute(name, "a string"))?;
    if text.trim().is_empty() {
        return Err(DefinitionError::EmptyAttribute(name));
    }
    Ok(text.to_owned())
}

fn optional_string(
    object: &Map<String, Value>,
    key: &str,
    name: &'static str,
) -> Result<Option<String>, DefinitionError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(DefinitionError::InvalidAttribute(name, "a string")),
    }
}

fn optional_schema(
    object: &Map<String, Value>,
    key: &str,
    name: &'static str,
) -> Result<Option<String>, DefinitionError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(schema)) => Ok(Some(schema.clone())),
        Some(value @ (Value::Object(_) | Value::Array(_))) => {
            let mut encoded = Vec::new();
            write_canonical(value, &mut encoded);
            Ok(Some(
                String::from_utf8(encoded).expect("canonical JSON always produces valid UTF-8"),
            ))
        }
        Some(_) => Err(DefinitionError::InvalidAttribute(
            name,
            "a JSON Schema string or object",
        )),
    }
}

fn optional_string_array(
    object: &Map<String, Value>,
    key: &str,
    name: &'static str,
) -> Result<Vec<String>, DefinitionError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or(DefinitionError::InvalidAttribute(
                        name,
                        "an array of strings",
                    ))
            })
            .collect(),
        Some(_) => Err(DefinitionError::InvalidAttribute(
            name,
            "an array of strings",
        )),
    }
}

fn optional_bool(
    object: &Map<String, Value>,
    key: &str,
    name: &'static str,
) -> Result<bool, DefinitionError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(flag)) => Ok(*flag),
        Some(_) => Err(DefinitionError::InvalidAttribute(name, "a boolean")),
    }
}

fn required_u32(
    object: &Map<String, Value>,
    key: &str,
    name: &'static str,
) -> Result<u32, DefinitionError> {
    let value = object
        .get(key)
        .ok_or(DefinitionError::MissingAttribute(name))?;
    match value {
        Value::Number(number) if number.is_u64() || number.is_i64() => number
            .as_u64()
            .and_then(|number| u32::try_from(number).ok())
            .ok_or(DefinitionError::InvalidAttribute(
                name,
                "an unsigned 32-bit integer",
            )),
        _ => Err(DefinitionError::InvalidAttribute(
            name,
            "an unsigned 32-bit integer",
        )),
    }
}

fn optional_i32(
    object: &Map<String, Value>,
    key: &str,
    name: &'static str,
) -> Result<Option<i32>, DefinitionError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) if number.is_i64() || number.is_u64() => number
            .as_i64()
            .and_then(|number| i32::try_from(number).ok())
            .map(Some)
            .ok_or(DefinitionError::InvalidAttribute(
                name,
                "a signed 32-bit integer",
            )),
        Some(_) => Err(DefinitionError::InvalidAttribute(
            name,
            "a signed 32-bit integer",
        )),
    }
}

fn field_to_proto(field: &FieldDef) -> Result<v1::FieldDef, DefinitionError> {
    Ok(v1::FieldDef {
        id: field.id,
        key: field.key.clone(),
        r#type: field_value_type(field)?.to_proto() as i32,
        unit: field.unit.clone(),
        scale: field.scale,
        description: None,
    })
}

fn action_to_proto(action: &ActionDef) -> v1::ActionDef {
    v1::ActionDef {
        id: action.id,
        key: action.key.clone(),
        params_schema_json: action
            .params_schema
            .as_ref()
            .map(|schema| schema.as_bytes().to_vec())
            .unwrap_or_default(),
        permissions: action.permissions.clone(),
        requires_confirmation: action.requires_confirmation,
        label: None,
        description: None,
    }
}

fn encode_sint(field: &FieldDef, value: &Value) -> Result<i64, DefinitionError> {
    match value {
        Value::Number(number) if number.is_i64() => Ok(number
            .as_i64()
            .expect("is_i64 guarantees an i64 representation")),
        Value::Number(number) if number.is_u64() => {
            let unsigned = number
                .as_u64()
                .expect("is_u64 guarantees a u64 representation");
            i64::try_from(unsigned).map_err(|_| out_of_range(field.id, FieldValueType::Sint))
        }
        Value::Number(number) if number.is_f64() => {
            let scaled = scale_float(
                field,
                FieldValueType::Sint,
                number.as_f64().unwrap_or(f64::NAN),
            )?;
            if scaled < i64::MIN as f64 || scaled > MAX_SAFE_INTEGER {
                return Err(out_of_range(field.id, FieldValueType::Sint));
            }
            Ok(scaled as i64)
        }
        _ => Err(mismatch(field.id, FieldValueType::Sint, value)),
    }
}

fn encode_uint(field: &FieldDef, value: &Value) -> Result<u64, DefinitionError> {
    match value {
        Value::Number(number) if number.is_u64() => Ok(number
            .as_u64()
            .expect("is_u64 guarantees a u64 representation")),
        Value::Number(number) if number.is_i64() => {
            let signed = number
                .as_i64()
                .expect("is_i64 guarantees an i64 representation");
            u64::try_from(signed).map_err(|_| out_of_range(field.id, FieldValueType::Uint))
        }
        Value::Number(number) if number.is_f64() => {
            let scaled = scale_float(
                field,
                FieldValueType::Uint,
                number.as_f64().unwrap_or(f64::NAN),
            )?;
            if !(0.0..=MAX_SAFE_INTEGER).contains(&scaled) {
                return Err(out_of_range(field.id, FieldValueType::Uint));
            }
            Ok(scaled as u64)
        }
        _ => Err(mismatch(field.id, FieldValueType::Uint, value)),
    }
}

fn encode_real(field: &FieldDef, value: &Value) -> Result<f64, DefinitionError> {
    match value {
        Value::Number(number) => number
            .as_f64()
            .filter(|real| real.is_finite())
            .ok_or_else(|| out_of_range(field.id, FieldValueType::Real)),
        _ => Err(mismatch(field.id, FieldValueType::Real, value)),
    }
}

fn encode_enum(field: &FieldDef, value: &Value) -> Result<i32, DefinitionError> {
    match value {
        Value::Number(number) if number.is_i64() => number
            .as_i64()
            .and_then(|number| i32::try_from(number).ok())
            .ok_or_else(|| out_of_range(field.id, FieldValueType::Enum)),
        Value::Number(number) if number.is_u64() => number
            .as_u64()
            .and_then(|number| i32::try_from(number).ok())
            .ok_or_else(|| out_of_range(field.id, FieldValueType::Enum)),
        _ => Err(mismatch(field.id, FieldValueType::Enum, value)),
    }
}

fn encode_bytes(field: &FieldDef, value: &Value) -> Result<Vec<u8>, DefinitionError> {
    match value {
        Value::String(text) => Ok(text.as_bytes().to_vec()),
        Value::Array(items) => items
            .iter()
            .map(|item| {
                item.as_u64()
                    .and_then(|byte| u8::try_from(byte).ok())
                    .ok_or_else(|| {
                        invalid(field.id, "bytes arrays must contain integers from 0 to 255")
                    })
            })
            .collect(),
        _ => Err(mismatch(field.id, FieldValueType::Bytes, value)),
    }
}

fn encode_timestamp(field: &FieldDef, value: &Value) -> Result<i64, DefinitionError> {
    match value {
        Value::Number(number) if number.is_i64() => Ok(number
            .as_i64()
            .expect("is_i64 guarantees an i64 representation")),
        Value::Number(number) if number.is_u64() => {
            let unsigned = number
                .as_u64()
                .expect("is_u64 guarantees a u64 representation");
            i64::try_from(unsigned).map_err(|_| out_of_range(field.id, FieldValueType::Timestamp))
        }
        _ => Err(mismatch(field.id, FieldValueType::Timestamp, value)),
    }
}

fn scale_float(
    field: &FieldDef,
    value_type: FieldValueType,
    value: f64,
) -> Result<f64, DefinitionError> {
    if !value.is_finite() {
        return Err(out_of_range(field.id, value_type));
    }

    let scale = field.scale.map(f64::from).unwrap_or(1.0);
    let scaled = value * scale;
    if !scaled.is_finite() {
        return Err(out_of_range(field.id, value_type));
    }

    let rounded = scaled.round();
    let tolerance = 1e-9 * scaled.abs().max(1.0);
    if (scaled - rounded).abs() > tolerance {
        return Err(invalid(
            field.id,
            format!(
                "value {value} is not exactly representable at scale {}",
                field.scale.unwrap_or(1)
            ),
        ));
    }

    if rounded.abs() > MAX_SAFE_INTEGER {
        return Err(invalid(
            field.id,
            "value is outside the exactly representable integer range",
        ));
    }

    Ok(rounded)
}

fn mismatch(field_id: u32, expected: FieldValueType, value: &Value) -> DefinitionError {
    DefinitionError::FieldTypeMismatch {
        field_id,
        expected: expected.as_str(),
        actual: json_kind(value),
    }
}

fn proto_mismatch(
    field_id: u32,
    expected: FieldValueType,
    actual: &'static str,
) -> DefinitionError {
    DefinitionError::FieldTypeMismatch {
        field_id,
        expected: expected.as_str(),
        actual,
    }
}

fn out_of_range(field_id: u32, expected: FieldValueType) -> DefinitionError {
    DefinitionError::InvalidFieldValue {
        field_id,
        message: format!("value is out of range for {}", expected.as_str()),
    }
}

fn invalid(field_id: u32, message: impl Into<String>) -> DefinitionError {
    DefinitionError::InvalidFieldValue {
        field_id,
        message: message.into(),
    }
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn proto_variant_name(value: &field_value::Value) -> &'static str {
    match value {
        field_value::Value::Integer(_) => "sint",
        field_value::Value::Unsigned(_) => "uint",
        field_value::Value::Real(_) => "real",
        field_value::Value::Flag(_) => "bool",
        field_value::Value::Text(_) => "string",
        field_value::Value::EnumValue(_) => "enum",
        field_value::Value::Blob(_) => "bytes",
        field_value::Value::TimestampMicros(_) => "timestamp",
    }
}

fn number_from_i64(value: i64) -> Value {
    Value::Number(Number::from(value))
}

fn number_from_u64(value: u64) -> Value {
    Value::Number(Number::from(value))
}

fn decode_blob(blob: &[u8]) -> Value {
    match std::str::from_utf8(blob) {
        Ok(text) => Value::String(text.to_owned()),
        Err(_) => Value::Array(
            blob.iter()
                .map(|byte| Value::Number(Number::from(*byte)))
                .collect(),
        ),
    }
}

fn canonical_bytes(value: &Value) -> Vec<u8> {
    let mut encoded = Vec::new();
    write_canonical(value, &mut encoded);
    encoded
}

fn write_canonical(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(true) => out.extend_from_slice(b"true"),
        Value::Bool(false) => out.extend_from_slice(b"false"),
        Value::Number(number) => write_canonical_number(number, out),
        Value::String(text) => {
            let encoded = serde_json::to_string(text)
                .expect("serializing a JSON string into a vector cannot fail");
            out.extend_from_slice(encoded.as_bytes());
        }
        Value::Array(items) => {
            out.push(b'[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write_canonical(item, out);
            }
            out.push(b']');
        }
        Value::Object(object) => {
            out.push(b'{');
            let mut keys: Vec<&String> = object.keys().collect();
            keys.sort_unstable();
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                let encoded = serde_json::to_string(key)
                    .expect("serializing a JSON object key into a vector cannot fail");
                out.extend_from_slice(encoded.as_bytes());
                out.push(b':');
                write_canonical(&object[key], out);
            }
            out.push(b'}');
        }
    }
}

fn write_canonical_number(number: &Number, out: &mut Vec<u8>) {
    if let Some(integer) = number.as_i64() {
        out.extend_from_slice(integer.to_string().as_bytes());
        return;
    }
    if let Some(integer) = number.as_u64() {
        out.extend_from_slice(integer.to_string().as_bytes());
        return;
    }

    if let Some(real) = number.as_f64() {
        // Canonicalize equivalent numeric spellings for exactly representable
        // integral values (`1.0` becomes `1`, `-0.0` becomes `0`).
        if real == 0.0 {
            out.push(b'0');
            return;
        }
        if real.is_finite() && real.fract() == 0.0 && real.abs() <= MAX_SAFE_INTEGER {
            out.extend_from_slice((real as i64).to_string().as_bytes());
            return;
        }
    }

    let text = number.to_string();
    if let Some((mantissa, exponent)) = text.split_once(['e', 'E']) {
        // Match the controller canonicalizer: no leading `+`, no redundant
        // exponent zeros, lower-case `e` (`1e+20` -> `1e20`, `1e-07` -> `1e-7`).
        let (sign, digits) = match exponent.strip_prefix('-') {
            Some(digits) => ("-", digits),
            None => ("", exponent.strip_prefix('+').unwrap_or(exponent)),
        };
        let digits = digits.trim_start_matches('0');
        let digits = if digits.is_empty() { "0" } else { digits };
        out.extend_from_slice(format!("{mantissa}e{sign}{digits}").as_bytes());
    } else {
        out.extend_from_slice(text.as_bytes());
    }
}
