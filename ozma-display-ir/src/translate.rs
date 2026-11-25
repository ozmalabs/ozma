// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! IR to `ScreenLayout`, `DisplayDefinition`, and compact projection mapping.
//!
//! The mappings are deliberately additive. They produce ordinary
//! `ScreenLayout` documents (`id`, optional `fields`, and `widgets`) and a
//! definition document whose `layout` is that screen layout, so existing
//! `DisplayDefinition` parsers continue to work. Full IR and source data live in
//! the asset pack; only a compact projection is written to `meta.ozma_ir`.
//!
//! Unknown extension data is never flattened into the wire layout. The
//! reserved `extensions.screen_layout` member maps back to the top-level layout
//! and to the originating widget, which lets the Python round-trip helper
//! preserve fields that this typed projection does not model.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

use crate::{Fidelity, IrResult, TranslationReport, WidgetIR, IR_SCHEMA};

/// Reserved extension member used by the Python `from_screen_layout` helper.
pub const SCREEN_LAYOUT_EXTENSION: &str = "screen_layout";

impl WidgetIR {
    /// Map this widget to a `ScreenLayout`-compatible JSON document.
    pub fn to_screen_layout(&self) -> Value {
        widget_irs_to_screen_layout(&self.id, std::slice::from_ref(self))
    }

    /// Map this widget after validating it.
    pub fn to_screen_layout_checked(&self) -> IrResult<Value> {
        self.validate()?;
        Ok(self.to_screen_layout())
    }

    /// Map this widget to a definition document.
    pub fn to_definition_json(&self, version: u32) -> Value {
        widget_irs_to_definition_json(&self.id, version, std::slice::from_ref(self))
    }

    /// Map this widget to a definition document with an explicit screen id.
    pub fn to_definition_json_with_id(&self, definition_id: &str, version: u32) -> Value {
        widget_irs_to_definition_json(definition_id, version, std::slice::from_ref(self))
    }

    /// Map this widget to a definition document after validating it.
    pub fn to_definition_json_checked(&self, version: u32) -> IrResult<Value> {
        self.validate()?;
        Ok(self.to_definition_json(version))
    }

    /// Compact projection for `DisplayDefinition.meta.ozma_ir`.
    pub fn compact_projection(&self) -> Value {
        compact_projection(std::slice::from_ref(self))
    }
}

/// Map one widget to a `ScreenLayout`-compatible JSON document.
pub fn to_screen_layout(widget: &WidgetIR) -> Value {
    widget_irs_to_screen_layout(&widget.id, std::slice::from_ref(widget))
}

/// Map one widget to a `ScreenLayout`-compatible JSON document after validation.
pub fn to_screen_layout_checked(widget: &WidgetIR) -> IrResult<Value> {
    widget.validate()?;
    Ok(to_screen_layout(widget))
}

/// Map a set of widgets to a `ScreenLayout`-compatible JSON document.
pub fn widget_irs_to_screen_layout(layout_id: &str, widgets: &[WidgetIR]) -> Value {
    let fields = compile_fields(widgets);
    let field_ids: BTreeMap<&str, u32> = fields
        .iter()
        .map(|field| (field.key.as_str(), field.id))
        .collect();
    let actions = compile_actions(widgets);
    let action_ids: BTreeMap<&str, u32> = actions
        .iter()
        .map(|action| (action.key.as_str(), action.id))
        .collect();

    let mut width: f64 = 480.0;
    let mut height: f64 = 480.0;
    let mut widget_docs = Vec::with_capacity(widgets.len());
    for widget in widgets {
        let (doc, right, bottom) = screen_widget_document(widget, &field_ids, &action_ids);
        width = width.max(right);
        height = height.max(bottom);
        widget_docs.push(doc);
    }

    let mut layout = Map::new();
    layout.insert("id".to_owned(), Value::String(layout_id.to_owned()));
    layout.insert("name".to_owned(), Value::String(layout_id.to_owned()));
    layout.insert("width".to_owned(), Value::from(width.round() as u64));
    layout.insert("height".to_owned(), Value::from(height.round() as u64));
    layout.insert("widgets".to_owned(), Value::Array(widget_docs));
    if !fields.is_empty() {
        layout.insert(
            "fields".to_owned(),
            Value::Array(fields.iter().map(CompiledField::to_value).collect()),
        );
    }
    merge_screen_layout_extensions(&mut layout, widgets);
    Value::Object(layout)
}

/// Map a set of widgets to a definition document.
///
/// The result has the canonical top-level `id`, `version`, `layout`, `fields`,
/// and `actions` members plus a compact `meta.ozma_ir` projection.
pub fn widget_irs_to_definition_json(
    definition_id: &str,
    version: u32,
    widgets: &[WidgetIR],
) -> Value {
    let layout = widget_irs_to_screen_layout(definition_id, widgets);
    let fields = compile_fields(widgets);
    let actions = compile_actions(widgets);

    let mut document = Map::new();
    document.insert("id".to_owned(), Value::String(definition_id.to_owned()));
    document.insert("version".to_owned(), Value::from(version));
    document.insert("layout".to_owned(), layout);
    document.insert(
        "fields".to_owned(),
        Value::Array(fields.iter().map(CompiledField::to_definition).collect()),
    );
    document.insert(
        "actions".to_owned(),
        Value::Array(actions.iter().map(CompiledAction::to_definition).collect()),
    );

    let mut meta = Map::new();
    meta.insert("ozma_ir".to_owned(), compact_projection(widgets));
    document.insert("meta".to_owned(), Value::Object(meta));
    Value::Object(document)
}

/// Compact IR projection for `DisplayDefinition.meta.ozma_ir`.
///
/// This is intentionally small: full source, parsed documents, and assets stay
/// in the asset pack. The wire definition only carries identity, semantic
/// binding, source hash, and asset identifiers.
pub fn compact_projection(widgets: &[WidgetIR]) -> Value {
    let mut projection = Map::new();
    projection.insert("schema".to_owned(), Value::String(IR_SCHEMA.to_owned()));
    projection.insert(
        "widgets".to_owned(),
        Value::Array(widgets.iter().map(compact_widget).collect()),
    );
    projection.insert("lossless".to_owned(), Value::Bool(true));
    Value::Object(projection)
}

/// Definition document for one widget.
pub fn to_definition_json(widget: &WidgetIR, version: u32) -> Value {
    widget_irs_to_definition_json(&widget.id, version, std::slice::from_ref(widget))
}

/// Definition document for one widget with an explicit screen id.
pub fn to_definition_json_with_id(widget: &WidgetIR, definition_id: &str, version: u32) -> Value {
    widget_irs_to_definition_json(definition_id, version, std::slice::from_ref(widget))
}

/// Validate then map one widget to a definition document.
pub fn to_definition_json_checked(widget: &WidgetIR, version: u32) -> IrResult<Value> {
    widget.validate()?;
    Ok(to_definition_json(widget, version))
}

/// Build a minimal export report for a widget and target.
///
/// Adapters should normally construct their own report; this helper exists so
/// the mandatory-report invariant has a single tested shape.
pub fn export_report(
    widget: &WidgetIR,
    target_format: &str,
    translator: &str,
    translator_version: &str,
    fidelity: Fidelity,
) -> TranslationReport {
    TranslationReport {
        direction: crate::TranslationDirection::Export,
        source_format: widget.source.format.clone(),
        target_format: target_format.to_owned(),
        translator: translator.to_owned(),
        translator_version: translator_version.to_owned(),
        fidelity,
        converted: vec![format!("widget:{}", widget.kind)],
        approximated: Vec::new(),
        dropped: Vec::new(),
        warnings: Vec::new(),
    }
}

/// Source format used when an IR originates from an existing Ozma definition.
pub const SOURCE_FORMAT_OZMA: &str = "ozma_display_definition";

#[derive(Clone, Debug)]
struct CompiledField {
    id: u32,
    key: String,
    value_type: String,
    unit: Option<String>,
    scale: Option<i64>,
}

impl CompiledField {
    fn to_definition(&self) -> Value {
        let mut entry = Map::new();
        entry.insert("id".to_owned(), Value::from(self.id));
        entry.insert("key".to_owned(), Value::String(self.key.clone()));
        entry.insert("type".to_owned(), Value::String(self.value_type.clone()));
        if let Some(unit) = &self.unit {
            entry.insert("unit".to_owned(), Value::String(unit.clone()));
        }
        if let Some(scale) = self.scale {
            entry.insert("scale".to_owned(), Value::from(scale));
        }
        Value::Object(entry)
    }

    fn to_value(&self) -> Value {
        self.to_definition()
    }
}

#[derive(Clone, Debug)]
struct CompiledAction {
    id: u32,
    key: String,
    label: Option<String>,
    permissions: Vec<String>,
}

impl CompiledAction {
    fn to_definition(&self) -> Value {
        let mut entry = Map::new();
        entry.insert("id".to_owned(), Value::from(self.id));
        entry.insert("key".to_owned(), Value::String(self.key.clone()));
        entry.insert("params_schema".to_owned(), Value::Object(Map::new()));
        if let Some(label) = &self.label {
            entry.insert("label".to_owned(), Value::String(label.clone()));
        }
        if !self.permissions.is_empty() {
            entry.insert(
                "permissions".to_owned(),
                Value::Array(
                    self.permissions
                        .iter()
                        .cloned()
                        .map(Value::String)
                        .collect(),
                ),
            );
        }
        Value::Object(entry)
    }
}

fn compile_fields(widgets: &[WidgetIR]) -> Vec<CompiledField> {
    let mut bindings: BTreeMap<&str, &WidgetIR> = BTreeMap::new();
    for widget in widgets {
        bindings
            .entry(widget.binding.metric.as_str())
            .or_insert(widget);
    }

    let mut used = BTreeSet::new();
    let mut ids: BTreeMap<&str, u32> = BTreeMap::new();
    for (key, widget) in &bindings {
        if let Some(id) = widget.binding.field_id {
            if id > 0 && used.insert(id) {
                ids.insert(key, id);
            }
        }
    }
    let mut next = 1u32;
    for key in bindings.keys() {
        if ids.contains_key(key) {
            continue;
        }
        while used.contains(&next) {
            next = next.saturating_add(1);
        }
        used.insert(next);
        ids.insert(key, next);
        next = next.saturating_add(1);
    }

    bindings
        .iter()
        .map(|(key, widget)| CompiledField {
            id: ids[key],
            key: (*key).to_owned(),
            value_type: value_type_for(widget),
            unit: widget.binding.unit.clone(),
            scale: widget
                .binding
                .scale
                .filter(|scale| scale.is_finite() && scale.fract() == 0.0)
                .map(|scale| scale as i64),
        })
        .collect()
}

fn compile_actions(widgets: &[WidgetIR]) -> Vec<CompiledAction> {
    let mut actions: BTreeMap<String, (&WidgetIR, &crate::ActionRef)> = BTreeMap::new();
    for widget in widgets {
        for action in &widget.actions {
            actions
                .entry(action.key.clone())
                .or_insert((widget, action));
        }
    }

    let mut used = BTreeSet::new();
    let mut ids: BTreeMap<String, u32> = BTreeMap::new();
    for (key, (_, action)) in &actions {
        if let Some(id) = action.action_id {
            if id > 0 && used.insert(id) {
                ids.insert(key.clone(), id);
            }
        }
    }
    let mut next = 1u32;
    for key in actions.keys() {
        if ids.contains_key(key) {
            continue;
        }
        while used.contains(&next) {
            next = next.saturating_add(1);
        }
        used.insert(next);
        ids.insert(key.clone(), next);
        next = next.saturating_add(1);
    }

    actions
        .iter()
        .map(|(key, (_, action))| CompiledAction {
            id: ids[key],
            key: key.clone(),
            label: action.label.clone(),
            permissions: action.permissions.clone(),
        })
        .collect()
}

fn value_type_for(widget: &WidgetIR) -> String {
    if let Some(value_type) = &widget.binding.value_type {
        let normalized = value_type.to_ascii_lowercase();
        if !normalized.is_empty() {
            return normalized;
        }
    }
    match widget.kind.as_str() {
        "text" => "string".to_owned(),
        _ => "real".to_owned(),
    }
}

fn screen_widget_document(
    widget: &WidgetIR,
    field_ids: &BTreeMap<&str, u32>,
    action_ids: &BTreeMap<&str, u32>,
) -> (Value, f64, f64) {
    let layout = widget.layout.clone().unwrap_or_default();
    let x = layout.x.unwrap_or(0.0).round() as i64;
    let y = layout.y.unwrap_or(0.0).round() as i64;
    let width = layout.width.unwrap_or(120.0).round().max(1.0);
    let height = layout.height.unwrap_or(120.0).round().max(1.0);

    let mut doc = Map::new();
    doc.insert(
        "type".to_owned(),
        Value::String(screen_widget_type(&widget.kind).to_owned()),
    );
    doc.insert("ir_kind".to_owned(), Value::String(widget.kind.clone()));
    doc.insert("ir_id".to_owned(), Value::String(widget.id.clone()));
    doc.insert("x".to_owned(), Value::from(x));
    doc.insert("y".to_owned(), Value::from(y));
    doc.insert("w".to_owned(), Value::from(width as i64));
    doc.insert("h".to_owned(), Value::from(height as i64));
    doc.insert(
        "metric".to_owned(),
        Value::String(widget.binding.metric.clone()),
    );
    if let Some(field_id) = field_ids.get(widget.binding.metric.as_str()) {
        doc.insert("field".to_owned(), Value::from(*field_id));
    }
    if let Some(unit) = &widget.binding.unit {
        doc.insert("unit".to_owned(), Value::String(unit.clone()));
    }
    if let Some(label) = &widget.label {
        doc.insert("label".to_owned(), Value::String(label.clone()));
    }
    if let Some(format) = &widget.binding.format {
        doc.insert("format".to_owned(), Value::String(format.clone()));
    }
    if let Some(domain) = &widget.domain {
        if let Some(min) = domain.min {
            doc.insert("min".to_owned(), Value::from(min));
        }
        if let Some(max) = domain.max {
            doc.insert("max".to_owned(), Value::from(max));
        }
        for threshold in &domain.thresholds {
            match threshold.severity.to_ascii_lowercase().as_str() {
                "warning" | "warn" => {
                    if let Some(color) = &threshold.color {
                        doc.insert("warn_color".to_owned(), Value::String(color.clone()));
                    }
                }
                "critical" | "crit" | "error" => {
                    if let Some(color) = &threshold.color {
                        doc.insert("crit_color".to_owned(), Value::String(color.clone()));
                    }
                }
                _ => {}
            }
        }
    }
    if let Some(typography) = &widget.typography {
        if let Some(size) = typography.size_px {
            doc.insert("font_size".to_owned(), Value::from(size.round() as i64));
        }
    }
    if let Some(icon) = &widget.icon {
        if let Some(name) = &icon.name {
            doc.insert("icon".to_owned(), Value::String(name.clone()));
        } else if let Some(asset_id) = &icon.asset_id {
            doc.insert("icon_asset".to_owned(), Value::String(asset_id.clone()));
        }
    }
    if let Some(accessibility) = &widget.accessibility {
        if let Some(label) = &accessibility.label {
            doc.insert(
                "accessibility_label".to_owned(),
                Value::String(label.clone()),
            );
        }
    }
    if let Some(action) = widget.actions.first() {
        doc.insert("action".to_owned(), Value::String(action.key.clone()));
        if let Some(action_id) = action_ids.get(action.key.as_str()) {
            doc.insert("action_id".to_owned(), Value::from(*action_id));
        }
    }
    if let Some(history) = &widget.history {
        if let Some(window) = history.window_seconds {
            if window.fract() == 0.0 {
                doc.insert(
                    "history_seconds".to_owned(),
                    Value::from(window.round() as i64),
                );
            }
        }
    }
    merge_widget_extensions(&mut doc, widget);
    (Value::Object(doc), x as f64 + width, y as f64 + height)
}

fn screen_widget_type(kind: &str) -> &str {
    match kind {
        "text" => "label",
        "chart" => "sparkline",
        "status" => "icon",
        other => other,
    }
}

fn merge_screen_layout_extensions(layout: &mut Map<String, Value>, widgets: &[WidgetIR]) {
    if let Some(Value::Object(extensions)) = widgets
        .first()
        .and_then(|widget| widget.extensions.get(SCREEN_LAYOUT_EXTENSION))
    {
        if let Some(Value::Object(top_level)) = extensions.get("layout") {
            for (key, value) in top_level {
                if !is_reserved_layout_key(key) {
                    layout.insert(key.clone(), value.clone());
                }
            }
        }
    }
}

fn merge_widget_extensions(doc: &mut Map<String, Value>, widget: &WidgetIR) {
    if let Some(Value::Object(extensions)) = widget.extensions.get(SCREEN_LAYOUT_EXTENSION) {
        if let Some(Value::Object(widget_extra)) = extensions.get("widget") {
            for (key, value) in widget_extra {
                doc.insert(key.clone(), value.clone());
            }
        }
    }
}

fn is_reserved_layout_key(key: &str) -> bool {
    matches!(key, "id" | "width" | "height" | "widgets" | "fields")
}

fn compact_widget(widget: &WidgetIR) -> Value {
    let mut compact = Map::new();
    compact.insert("id".to_owned(), Value::String(widget.id.clone()));
    compact.insert("kind".to_owned(), Value::String(widget.kind.clone()));
    if let Some(role) = &widget.role {
        compact.insert("role".to_owned(), Value::String(role.clone()));
    }
    compact.insert(
        "metric".to_owned(),
        Value::String(widget.binding.metric.clone()),
    );
    if let Some(unit) = &widget.binding.unit {
        compact.insert("unit".to_owned(), Value::String(unit.clone()));
    }
    if let Some(label) = &widget.label {
        compact.insert("label".to_owned(), Value::String(label.clone()));
    }
    compact.insert(
        "source_format".to_owned(),
        Value::String(widget.source.format.clone()),
    );
    compact.insert(
        "source_hash".to_owned(),
        Value::String(widget.source.source_hash.clone()),
    );
    if let Some(vibe) = &widget.vibe {
        let mut vibe_projection = Map::new();
        vibe_projection.insert(
            "keywords".to_owned(),
            Value::Array(vibe.keywords.iter().cloned().map(Value::String).collect()),
        );
        vibe_projection.insert("confidence".to_owned(), Value::from(vibe.confidence));
        compact.insert("vibe".to_owned(), Value::Object(vibe_projection));
    }
    if let Some(shape) = &widget.shape {
        if let Ok(value) = serde_json::to_value(shape) {
            compact.insert("shape".to_owned(), value);
        }
    }
    let mut asset_ids: Vec<Value> = widget
        .assets
        .iter()
        .map(|asset| Value::String(asset.id.clone()))
        .collect();
    asset_ids.extend(
        widget
            .fonts
            .iter()
            .map(|font| Value::String(font.id.clone())),
    );
    if !asset_ids.is_empty() {
        compact.insert("asset_ids".to_owned(), Value::Array(asset_ids));
    }
    Value::Object(compact)
}
