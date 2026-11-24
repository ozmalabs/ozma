// SPDX-License-Identifier: AGPL-3.0-only
//! Display methods and content resolution.
//!
//! A [`DisplayMethod`] is a concrete way a device or graph service can present
//! content. Rendering tiers are hints only: a tier never implies that a
//! particular content type, schema, or interaction is supported. The
//! advertised methods are authoritative, so resolution always goes through
//! [`MethodRegistry`] or [`resolve_method`].

use serde::{Deserialize, Serialize};

/// Broad rendering strategy advertised by a device.
///
/// Tiers are negotiation hints. They do not, on their own, satisfy a
/// [`ContentRequirement`]; a matching [`DisplayMethod`] is still required.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RenderingTier {
    /// Raw pixels are pushed to the device.
    PixelSink,
    /// A pre-rendered UI surface is pushed to the device.
    RenderedUi,
    /// The device renders native widgets from scene data.
    NativeWidgets,
    /// The device renders a data-driven definition.
    DataDriven,
}

/// Where a display method executes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MethodLocation {
    /// The method runs on the device itself.
    Device,
    /// The method runs in a graph service, which renders for the device.
    GraphService,
    /// The method can run on either the device or a graph service.
    Either,
}

impl MethodLocation {
    /// Preference rank used by resolution. Device-local rendering is preferred
    /// over a service fallback; `Either` sits between the two.
    fn preference(self) -> u8 {
        match self {
            Self::Device => 0,
            Self::Either => 1,
            Self::GraphService => 2,
        }
    }
}

/// Interaction modalities a method can deliver.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Interaction {
    Pointer,
    Touch,
    Keyboard,
    Buttons,
    Gamepad,
    Pen,
    Focus,
}

/// A concrete presentation method advertised during negotiation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayMethod {
    /// Stable identifier, unique within the advertising device or service.
    pub id: String,
    /// Content types the method accepts, for example
    /// `application/vnd.ozma.scene+json`.
    #[serde(default)]
    pub accepts: Vec<String>,
    /// Data schemas the method can consume. Only checked when a requirement
    /// declares a schema.
    #[serde(default)]
    pub schemas: Vec<String>,
    pub location: MethodLocation,
    /// Output formats the method can produce.
    #[serde(default)]
    pub outputs: Vec<String>,
    /// Interaction modalities the method can deliver.
    #[serde(default)]
    pub interactions: Vec<Interaction>,
    /// Optional schema describing method-specific configuration.
    #[serde(default)]
    pub configuration_schema: Option<String>,
}

impl DisplayMethod {
    /// Returns true when this method advertises `content_type`.
    pub fn accepts_content_type(&self, content_type: &str) -> bool {
        self.accepts.iter().any(|accepted| accepted == content_type)
    }

    /// Returns true when this method advertises every required interaction.
    pub fn supports_all(&self, interactions: &[Interaction]) -> bool {
        interactions
            .iter()
            .all(|required| self.interactions.contains(required))
    }
}

/// One field in a [`DataSchema`] definition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataField {
    pub key: String,
    pub value_type: String,
    #[serde(default)]
    pub unit: Option<String>,
    #[serde(default)]
    pub enum_values: Vec<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: Option<String>,
}

/// A data schema a method can present, for example a telemetry layout.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataSchema {
    pub id: String,
    #[serde(default)]
    pub fields: Vec<DataField>,
    /// How values are updated, for example `push`, `poll`, or `delta`.
    pub update_mode: String,
    #[serde(default)]
    pub max_update_rate_hz: Option<u32>,
}

/// A compiled field-table entry from a display definition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldDef {
    /// Stable for the definition lifetime and never reused.
    pub id: u32,
    /// Namespaced key, for example `org.ozma/node.cpu_temp`.
    pub key: String,
    pub value_type: String,
    #[serde(default)]
    pub unit: Option<String>,
    /// Scaled-integer exponent, for example `100` for hundredths.
    #[serde(default)]
    pub scale: Option<i32>,
}

/// An action exposed by a display definition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionDef {
    pub id: u32,
    pub key: String,
    #[serde(default)]
    pub params_schema: Option<String>,
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default)]
    pub requires_confirmation: bool,
}

/// The action and scene node targeted by an interaction event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InteractionTarget {
    #[serde(default)]
    pub action_id: Option<u32>,
    #[serde(default)]
    pub node_id: Option<u64>,
    #[serde(default)]
    pub accessibility_label: Option<String>,
    #[serde(default)]
    pub cursor: Option<String>,
}

/// What a caller needs a display method to consume.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentRequirement {
    /// Required content type, for example `application/vnd.ozma.scene+json`.
    pub content_type: String,
    /// Optional data schema the method must advertise.
    #[serde(default)]
    pub schema: Option<String>,
    /// Interactions the method must support.
    #[serde(default)]
    pub interactions: Vec<Interaction>,
}

/// A [`DisplayMethod`] together with the device or graph service that
/// advertised it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisteredMethod {
    pub method: DisplayMethod,
    /// Set when the method is advertised by a device.
    #[serde(default)]
    pub device_id: Option<String>,
    /// Set when the method is advertised by a graph service.
    #[serde(default)]
    pub service_id: Option<String>,
}

impl RegisteredMethod {
    /// Registers a method advertised by a device.
    pub fn device(method: DisplayMethod, device_id: impl Into<String>) -> Self {
        Self {
            method,
            device_id: Some(device_id.into()),
            service_id: None,
        }
    }

    /// Registers a method advertised by a graph service.
    pub fn service(method: DisplayMethod, service_id: impl Into<String>) -> Self {
        Self {
            method,
            device_id: None,
            service_id: Some(service_id.into()),
        }
    }

    /// Where the method executes.
    pub fn location(&self) -> MethodLocation {
        self.method.location
    }
}

/// Registry of methods advertised by devices and graph services.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MethodRegistry {
    methods: Vec<RegisteredMethod>,
}

impl MethodRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers one advertised method.
    pub fn register(&mut self, registered: RegisteredMethod) {
        self.methods.push(registered);
    }

    /// All registered methods, in registration order.
    pub fn methods(&self) -> &[RegisteredMethod] {
        &self.methods
    }

    /// Selects the best method for `requirement`, if any.
    ///
    /// Returns `None` when no registered method accepts the content type,
    /// advertises the required schema, and supports every required
    /// interaction.
    pub fn resolve(&self, requirement: &ContentRequirement) -> Option<&RegisteredMethod> {
        resolve_method(&self.methods, requirement)
    }
}

/// Selects the best method for `requirement` without mutating any state.
///
/// Matching rules:
/// - the content type must appear in `method.accepts`;
/// - when `requirement.schema` is set, `method.schemas` must contain it;
/// - every interaction in `requirement.interactions` must be advertised;
/// - rendering tiers play no part: a tier hint alone never resolves.
///
/// Among matching methods, the winner is chosen by:
/// 1. exact schema match (always true for candidates when a schema is
///    required, so a method without the schema can never win);
/// 2. device location over graph-service location, with `Either` between;
/// 3. method id, then source ids, for deterministic ordering.
pub fn resolve_method<'a>(
    methods: &'a [RegisteredMethod],
    requirement: &ContentRequirement,
) -> Option<&'a RegisteredMethod> {
    methods
        .iter()
        .filter(|registered| matches_requirement(&registered.method, requirement))
        .min_by(|left, right| {
            rank(left, requirement)
                .cmp(&rank(right, requirement))
                .then_with(|| left.method.id.cmp(&right.method.id))
                .then_with(|| left.device_id.cmp(&right.device_id))
                .then_with(|| left.service_id.cmp(&right.service_id))
        })
}

fn matches_requirement(method: &DisplayMethod, requirement: &ContentRequirement) -> bool {
    method.accepts_content_type(&requirement.content_type)
        && requirement
            .schema
            .as_ref()
            .is_none_or(|schema| method.schemas.contains(schema))
        && method.supports_all(&requirement.interactions)
}

/// Ranking key; smaller is preferred. Schema exactness is part of the key so
/// it is explicit, even though eligibility already requires it when a schema
/// is declared.
fn rank(registered: &RegisteredMethod, requirement: &ContentRequirement) -> (u8, u8) {
    let schema_rank = if requirement.schema.is_some() { 0 } else { 1 };
    (schema_rank, registered.method.location.preference())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::{DisplayCapabilities, SCENE_VERSION};

    fn method(id: &str, location: MethodLocation) -> DisplayMethod {
        DisplayMethod {
            id: id.to_string(),
            accepts: vec!["application/vnd.ozma.scene+json".to_string()],
            schemas: vec!["org.ozma/telemetry".to_string()],
            location,
            outputs: vec!["scene".to_string()],
            interactions: vec![Interaction::Pointer, Interaction::Touch],
            configuration_schema: None,
        }
    }

    fn requirement() -> ContentRequirement {
        ContentRequirement {
            content_type: "application/vnd.ozma.scene+json".to_string(),
            schema: Some("org.ozma/telemetry".to_string()),
            interactions: vec![Interaction::Pointer, Interaction::Touch],
        }
    }

    #[test]
    fn exact_match_resolves() {
        let registered =
            RegisteredMethod::device(method("native", MethodLocation::Device), "dev-1");
        let mut registry = MethodRegistry::new();
        registry.register(registered.clone());

        assert_eq!(registry.resolve(&requirement()), Some(&registered));
    }

    #[test]
    fn schema_match_is_required_when_the_requirement_declares_one() {
        let mut without_schema = method("native", MethodLocation::Device);
        without_schema.schemas.clear();
        let registry = {
            let mut registry = MethodRegistry::new();
            registry.register(RegisteredMethod::device(without_schema, "dev-1"));
            registry
        };

        assert_eq!(registry.resolve(&requirement()), None);
    }

    #[test]
    fn schema_match_alone_does_not_replace_content_type_acceptance() {
        let mut schema_only = method("native", MethodLocation::Device);
        schema_only.accepts = vec!["image/png".to_string()];
        let registry = {
            let mut registry = MethodRegistry::new();
            registry.register(RegisteredMethod::device(schema_only, "dev-1"));
            registry
        };

        assert_eq!(registry.resolve(&requirement()), None);
    }

    #[test]
    fn unsupported_interaction_is_rejected() {
        let mut keyboard_only = method("keyboard", MethodLocation::Device);
        keyboard_only.interactions = vec![Interaction::Keyboard];
        let registry = {
            let mut registry = MethodRegistry::new();
            registry.register(RegisteredMethod::device(keyboard_only, "dev-1"));
            registry
        };

        assert_eq!(registry.resolve(&requirement()), None);
    }

    #[test]
    fn device_method_is_preferred_over_graph_service() {
        let registry = {
            let mut registry = MethodRegistry::new();
            registry.register(RegisteredMethod::service(
                method("a-service", MethodLocation::GraphService),
                "svc-1",
            ));
            registry.register(RegisteredMethod::device(
                method("z-device", MethodLocation::Device),
                "dev-1",
            ));
            registry
        };

        let resolved = registry.resolve(&requirement()).expect("a method matches");
        assert_eq!(resolved.method.id, "z-device");
        assert_eq!(resolved.location(), MethodLocation::Device);
    }

    #[test]
    fn requirement_without_schema_can_use_a_graph_service_fallback() {
        let requirement = ContentRequirement {
            schema: None,
            interactions: vec![Interaction::Pointer],
            ..requirement()
        };
        let service =
            RegisteredMethod::service(method("rendered", MethodLocation::GraphService), "svc-1");
        let registry = {
            let mut registry = MethodRegistry::new();
            registry.register(service.clone());
            registry
        };

        assert_eq!(registry.resolve(&requirement), Some(&service));
    }

    #[test]
    fn ties_are_broken_deterministically_by_id() {
        let registry = {
            let mut registry = MethodRegistry::new();
            registry.register(RegisteredMethod::device(
                method("method-b", MethodLocation::Device),
                "dev-2",
            ));
            registry.register(RegisteredMethod::device(
                method("method-a", MethodLocation::Device),
                "dev-1",
            ));
            registry
        };

        let resolved = registry.resolve(&requirement()).expect("a method matches");
        assert_eq!(resolved.method.id, "method-a");
    }

    #[test]
    fn tier_hint_alone_does_not_resolve() {
        let capabilities = DisplayCapabilities {
            scene_versions: vec![SCENE_VERSION],
            methods: Vec::new(),
            tiers: vec![RenderingTier::DataDriven, RenderingTier::NativeWidgets],
            schemas: vec!["org.ozma/telemetry".to_string()],
            max_update_rate_hz: Some(60),
            resource_budget_bytes: 1 << 20,
        };
        let registry = MethodRegistry::new();

        // The capabilities advertise a suitable tier, but no concrete method
        // can consume the content, so resolution must fail.
        assert!(capabilities.tiers.contains(&RenderingTier::DataDriven));
        assert_eq!(registry.resolve(&requirement()), None);
    }

    #[test]
    fn enums_use_snake_case_on_the_wire() {
        assert_eq!(
            serde_json::to_string(&RenderingTier::RenderedUi).unwrap(),
            "\"rendered_ui\""
        );
        assert_eq!(
            serde_json::to_string(&Interaction::Keyboard).unwrap(),
            "\"keyboard\""
        );
        assert_eq!(
            serde_json::to_string(&MethodLocation::GraphService).unwrap(),
            "\"graph_service\""
        );
    }

    #[test]
    fn display_method_round_trips_with_defaults() {
        let json = r#"{
            "id": "native",
            "location": "device",
            "accepts": ["application/vnd.ozma.scene+json"]
        }"#;
        let method: DisplayMethod = serde_json::from_str(json).unwrap();

        assert_eq!(method.id, "native");
        assert!(method.schemas.is_empty());
        assert!(method.interactions.is_empty());
        assert!(method.configuration_schema.is_none());
        assert_eq!(
            serde_json::from_str::<DisplayMethod>(&serde_json::to_string(&method).unwrap())
                .unwrap(),
            method
        );
    }
}
