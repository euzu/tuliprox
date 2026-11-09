//! `SchemaForm`: renders the settings JSON Schema a plugin ships (a draft 2020-12 **subset**)
//! with the existing form primitives.
//!
//! The component is controlled: it renders `value` and emits the whole edited
//! object through `on_change`. The schema analysis (what is supported, which
//! property is a secret, which default prefills, how a property is addressed)
//! is pure and lives in the first half of this file, apart from the rendering.
//!
//! Besides the validation keywords (checked by `shared::utils::validate_plugin_schema`),
//! a schema may carry UI hints, which never change what is valid:
//!
//! * `widget` — `textarea`, `code` (strings), `color` (strings), `slider`
//!   (integers and numbers with a `minimum` and a `maximum`), `radio` (enums),
//!   `duration` (integers in seconds, strings as `30m` or, with
//!   `format: duration`, as `PT30M`). A widget that does not fit the type is ignored.
//! * `enumLabels` — labels for an `enum` (or an array's `items.enum`): a
//!   list in the order of `enum`, or an object from value to label.
//! * `placeholder` — the text shown in an empty field.
//! * `helpUrl` — an `http(s)` link next to the field's label.
//! * `columns` — on an object (the root or a group): lay its fields out in
//!   1–4 columns. `row: true` on a property gives it a row of its own.
//! * `collapsed` — on a group: start collapsed.

use crate::{
    app::components::{
        input::Input, number_input::NumberInput, Accordion, AccordionPanel, AppIcon, DropDownOption, DropDownSelection,
        HideContent, IconButton, RadioButtonGroup, Select, Tag, TagList, TextArea, ToggleSwitch,
    },
    i18n::use_translation,
    model::{DialogAction, DialogActions, DialogResult},
    services::DialogService,
};
use serde_json::{Map, Number, Value};
use shared::{
    model::{PluginFieldErrorDto, PluginSecretDto},
    utils::PLUGIN_SCHEMA_MAX_DEPTH,
};
use std::rc::Rc;
use web_sys::{HtmlInputElement, InputEvent, MouseEvent};
use yew::{classes, component, html, use_context, use_state, Callback, Html, Properties, TargetCast};

// ---------------------------------------------------------------------------
// Schema analysis (pure)
// ---------------------------------------------------------------------------

/// Schema keywords the form cannot render. One of them anywhere in the schema
/// turns the whole form into a single error.
const UNSUPPORTED_KEYWORDS: [&str; 10] =
    ["oneOf", "anyOf", "allOf", "$ref", "if", "then", "else", "not", "dependentSchemas", "patternProperties"];

/// The path used for errors about the settings object as a whole.
pub const ROOT_PATH: &str = "(root)";

/// The most columns `columns` lays fields out in.
const MAX_COLUMNS: u8 = 4;

/// The type of one element of a list property.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarKind {
    Text,
    Integer,
    Number,
    Boolean,
}

impl ScalarKind {
    /// Parses what the user typed into a tag as a JSON value of this kind.
    fn parse(self, raw: &str) -> Option<Value> {
        let raw = raw.trim();
        match self {
            Self::Text => Some(Value::String(raw.to_owned())),
            Self::Integer => raw.parse::<i64>().ok().map(Value::from),
            Self::Number => raw.parse::<f64>().ok().and_then(Number::from_f64).map(Value::Number),
            Self::Boolean => raw.parse::<bool>().ok().map(Value::Bool),
        }
    }
}

/// How a field is edited, from `widget`. [`Widget::Default`] is the control
/// of the field's type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Widget {
    Default,
    Textarea,
    Code,
    Color,
    Slider,
    Radio,
    Duration,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FieldKind {
    Text {
        /// The schema's `format`, when it is not `password`.
        format: Option<String>,
        max_length: Option<u32>,
    },
    /// `writeOnly: true` or `format: "password"`.
    Secret,
    Integer {
        /// The lowest value allowed, `exclusiveMinimum` folded in.
        min: Option<i64>,
        /// The highest value allowed, `exclusiveMaximum` folded in.
        max: Option<i64>,
        /// `multipleOf`.
        step: Option<i64>,
    },
    Number {
        min: Option<f64>,
        max: Option<f64>,
        step: Option<f64>,
    },
    Boolean,
    Enum {
        values: Vec<Value>,
        /// One label per value, in the order of `values`.
        labels: Vec<String>,
    },
    List {
        element: ScalarKind,
        /// `uniqueItems`: a tag that is already in the list is not added.
        unique: bool,
        /// `maxItems`: no tag is added once the list is full.
        max_items: Option<usize>,
    },
    /// An array whose `items` is an `enum`: a pick of several values.
    MultiSelect {
        values: Vec<Value>,
        labels: Vec<String>,
    },
    /// An object property.
    Group {
        fields: Vec<FieldModel>,
        columns: u8,
        collapsed: bool,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct FieldModel {
    pub key: String,
    /// The property names from the root down.
    pub keys: Vec<String>,
    /// The secret-path grammar of the API: `a`, `a.b`.
    pub path: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub default: Option<Value>,
    pub required: bool,
    /// The type is a list of one type and `null`.
    pub nullable: bool,
    pub placeholder: Option<String>,
    /// An `http(s)` link next to the label.
    pub help_url: Option<String>,
    pub widget: Widget,
    /// The field takes a row of its own in a multi-column layout.
    pub full_row: bool,
    pub kind: FieldKind,
}

impl FieldModel {
    pub fn label(&self) -> &str { self.title.as_deref().filter(|title| !title.trim().is_empty()).unwrap_or(&self.key) }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FormModel {
    pub fields: Vec<FieldModel>,
    /// `columns` of the root.
    pub columns: u8,
}

impl Default for FormModel {
    fn default() -> Self { Self { fields: Vec::new(), columns: 1 } }
}

/// A schema construct the form cannot render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedSchema {
    /// The construct, for example `oneOf` or `array of objects`.
    pub construct: String,
    /// Where it occurs, in the same grammar as a field path.
    pub path: String,
}

impl UnsupportedSchema {
    fn new(construct: &str, path: &str) -> Self { Self { construct: construct.to_owned(), path: path.to_owned() } }

    pub fn describe(&self) -> String { format!("{} ({})", self.construct, self.path) }
}

/// The path of the property `key` below the object at `parent` (`""` for the root).
pub fn child_path(parent: &str, key: &str) -> String {
    if parent.is_empty() {
        key.to_owned()
    } else {
        format!("{parent}.{key}")
    }
}

/// The path of the elements of the list at `path`.
pub fn item_path(path: &str) -> String { format!("{path}[]") }

fn display_path(path: &str) -> &str {
    if path.is_empty() {
        ROOT_PATH
    } else {
        path
    }
}

/// Looks for an unsupported keyword in the schema node and in every sub-schema below it.
fn find_unsupported_keyword(node: &Value, path: &str) -> Option<UnsupportedSchema> {
    let object = node.as_object()?;
    for keyword in UNSUPPORTED_KEYWORDS {
        if object.contains_key(keyword) {
            return Some(UnsupportedSchema::new(keyword, display_path(path)));
        }
    }
    if let Some(properties) = object.get("properties").and_then(Value::as_object) {
        for (key, child) in properties {
            if let Some(found) = find_unsupported_keyword(child, &child_path(path, key)) {
                return Some(found);
            }
        }
    }
    if let Some(items) = object.get("items") {
        if let Some(found) = find_unsupported_keyword(items, &item_path(path)) {
            return Some(found);
        }
    }
    if let Some(additional) = object.get("additionalProperties") {
        if let Some(found) = find_unsupported_keyword(additional, path) {
            return Some(found);
        }
    }
    for definitions in ["$defs", "definitions"] {
        if let Some(definitions) = object.get(definitions).and_then(Value::as_object) {
            for child in definitions.values() {
                if let Some(found) = find_unsupported_keyword(child, path) {
                    return Some(found);
                }
            }
        }
    }
    None
}

fn is_secret_schema(node: &Map<String, Value>) -> bool {
    node.get("writeOnly").and_then(Value::as_bool) == Some(true)
        || node.get("format").and_then(Value::as_str) == Some("password")
}

fn integer_bound(node: &Map<String, Value>, key: &str) -> Option<i64> {
    let bound = node.get(key)?;
    bound.as_i64().or_else(|| bound.as_f64().filter(|f| f.fract() == 0.0 && f.abs() < 9.0e15).map(|f| f as i64))
}

/// The lowest (`lower`) or highest integer allowed by the inclusive and the
/// exclusive bound together.
fn integer_limit(node: &Map<String, Value>, inclusive: &str, exclusive: &str, lower: bool) -> Option<i64> {
    let inclusive = integer_bound(node, inclusive);
    let exclusive = node.get(exclusive).and_then(Value::as_f64).map(|bound| {
        // The first integer past the bound.
        let next = if lower { bound.floor() + 1.0 } else { bound.ceil() - 1.0 };
        next as i64
    });
    match (inclusive, exclusive) {
        (Some(a), Some(b)) => Some(if lower { a.max(b) } else { a.min(b) }),
        (a, b) => a.or(b),
    }
}

fn scalar_kind_of(type_name: &str) -> Option<ScalarKind> {
    match type_name {
        "string" => Some(ScalarKind::Text),
        "integer" => Some(ScalarKind::Integer),
        "number" => Some(ScalarKind::Number),
        "boolean" => Some(ScalarKind::Boolean),
        _ => None,
    }
}

fn non_blank_str<'a>(node: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    node.get(key).and_then(Value::as_str).filter(|text| !text.trim().is_empty())
}

/// The type a property declares, and whether `null` is allowed besides it.
/// `["string", "null"]` is a nullable string; a list of several other types is
/// not supported.
fn declared_type<'a>(node: &'a Map<String, Value>, path: &str) -> Result<(Option<&'a str>, bool), UnsupportedSchema> {
    match node.get("type") {
        None => Ok((None, false)),
        Some(Value::String(name)) => Ok((Some(name.as_str()), false)),
        Some(Value::Array(names)) => {
            let names: Option<Vec<&str>> = names.iter().map(Value::as_str).collect();
            let names = names.ok_or_else(|| UnsupportedSchema::new("type list with a non-string entry", path))?;
            let nullable = names.contains(&"null");
            let others: Vec<&str> = names.iter().copied().filter(|name| *name != "null").collect();
            match others.as_slice() {
                [single] => Ok((Some(single), nullable)),
                [] => Err(UnsupportedSchema::new("type null", path)),
                _ => Err(UnsupportedSchema::new("type given as a list of several types", path)),
            }
        }
        Some(_) => Err(UnsupportedSchema::new("type that is not a name", path)),
    }
}

/// The labels of `values`: `enumLabels` as a list in the order of `values`,
/// or as an object from a value's text to its label; else the value's text.
fn enum_labels(nodes: &[&Map<String, Value>], values: &[Value]) -> Vec<String> {
    let given = nodes.iter().find_map(|node| node.get("enumLabels"));
    values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let label = match given {
                Some(Value::Array(labels)) => labels.get(index).and_then(Value::as_str),
                Some(Value::Object(labels)) => labels.get(&scalar_text(value)).and_then(Value::as_str),
                _ => None,
            };
            label.filter(|label| !label.trim().is_empty()).map_or_else(|| scalar_text(value), str::to_owned)
        })
        .collect()
}

fn scalar_enum(values: &Value, path: &str) -> Result<Vec<Value>, UnsupportedSchema> {
    values
        .as_array()
        .filter(|values| {
            !values.is_empty()
                && values.iter().all(|v| matches!(v, Value::String(_) | Value::Number(_) | Value::Bool(_)))
        })
        .cloned()
        .ok_or_else(|| UnsupportedSchema::new("enum of non-scalar values", path))
}

fn build_list_kind(node: &Map<String, Value>, path: &str) -> Result<FieldKind, UnsupportedSchema> {
    let Some(items) = node.get("items").and_then(Value::as_object) else {
        return Err(UnsupportedSchema::new("array without an items schema", path));
    };
    if let Some(values) = items.get("enum") {
        let values = scalar_enum(values, &item_path(path))?;
        let labels = enum_labels(&[node, items], &values);
        return Ok(FieldKind::MultiSelect { values, labels });
    }
    let (item_type, _) = declared_type(items, &item_path(path))?;
    let element = match item_type {
        Some("object") => return Err(UnsupportedSchema::new("array of objects", path)),
        Some("array") => return Err(UnsupportedSchema::new("array of arrays", path)),
        Some(name) => {
            scalar_kind_of(name).ok_or_else(|| UnsupportedSchema::new(&format!("array of type {name}"), path))?
        }
        None if items.contains_key("properties") => return Err(UnsupportedSchema::new("array of objects", path)),
        None => return Err(UnsupportedSchema::new("array items without a type", path)),
    };
    Ok(FieldKind::List {
        element,
        unique: node.get("uniqueItems").and_then(Value::as_bool) == Some(true),
        max_items: node.get("maxItems").and_then(Value::as_u64).and_then(|max| usize::try_from(max).ok()),
    })
}

/// `columns` of an object: 1 to [`MAX_COLUMNS`], 1 when absent or out of range.
fn columns_of(node: &Map<String, Value>) -> u8 {
    node.get("columns")
        .and_then(Value::as_u64)
        .and_then(|columns| u8::try_from(columns).ok())
        .filter(|columns| (1..=MAX_COLUMNS).contains(columns))
        .unwrap_or(1)
}

/// `widget`, kept only where it fits the field's type.
fn widget_for(node: &Map<String, Value>, kind: &FieldKind) -> Widget {
    match (node.get("widget").and_then(Value::as_str), kind) {
        (Some("textarea"), FieldKind::Text { .. }) => Widget::Textarea,
        (Some("code"), FieldKind::Text { .. }) => Widget::Code,
        (Some("color"), FieldKind::Text { .. }) => Widget::Color,
        (Some("duration"), FieldKind::Text { .. } | FieldKind::Integer { .. }) => Widget::Duration,
        (Some("radio"), FieldKind::Enum { .. }) => Widget::Radio,
        (
            Some("slider"),
            FieldKind::Integer { min: Some(_), max: Some(_), .. }
            | FieldKind::Number { min: Some(_), max: Some(_), .. },
        ) => Widget::Slider,
        _ => Widget::Default,
    }
}

/// An `helpUrl` that is an `http(s)` URL; anything else (a `javascript:`
/// URL, a relative path) is dropped.
fn help_url_of(node: &Map<String, Value>) -> Option<String> {
    non_blank_str(node, "helpUrl")
        .map(str::trim)
        .filter(|url| {
            let lower = url.to_ascii_lowercase();
            (lower.starts_with("https://") || lower.starts_with("http://")) && !url.contains(char::is_whitespace)
        })
        .map(str::to_owned)
}

/// `depth` is the number of object levels the property sits in: 0 at the root.
fn build_field(
    key: &str,
    parent_keys: &[String],
    schema: &Value,
    required: bool,
    depth: usize,
) -> Result<FieldModel, UnsupportedSchema> {
    let mut keys = parent_keys.to_vec();
    keys.push(key.to_owned());
    let path = keys.join(".");
    let Some(node) = schema.as_object() else {
        return Err(UnsupportedSchema::new("boolean schema", &path));
    };
    let (type_name, nullable) = declared_type(node, &path)?;

    let kind = if let Some(values) = node.get("enum") {
        let values = scalar_enum(values, &path)?;
        let labels = enum_labels(&[node], &values);
        FieldKind::Enum { values, labels }
    } else {
        match type_name {
            Some("string") if is_secret_schema(node) => FieldKind::Secret,
            Some("string") => FieldKind::Text {
                format: non_blank_str(node, "format").map(str::to_owned),
                max_length: node.get("maxLength").and_then(Value::as_u64).and_then(|max| u32::try_from(max).ok()),
            },
            Some("integer") => FieldKind::Integer {
                min: integer_limit(node, "minimum", "exclusiveMinimum", true),
                max: integer_limit(node, "maximum", "exclusiveMaximum", false),
                step: integer_bound(node, "multipleOf").filter(|step| *step > 0),
            },
            Some("number") => FieldKind::Number {
                min: node.get("minimum").or_else(|| node.get("exclusiveMinimum")).and_then(Value::as_f64),
                max: node.get("maximum").or_else(|| node.get("exclusiveMaximum")).and_then(Value::as_f64),
                step: node.get("multipleOf").and_then(Value::as_f64).filter(|step| *step > 0.0),
            },
            Some("boolean") => FieldKind::Boolean,
            Some("array") => build_list_kind(node, &path)?,
            Some("object") => {
                if depth >= PLUGIN_SCHEMA_MAX_DEPTH {
                    return Err(UnsupportedSchema::new("object nested too deep", &path));
                }
                FieldKind::Group {
                    fields: build_properties(node, &keys, depth + 1)?,
                    columns: columns_of(node),
                    collapsed: node.get("collapsed").and_then(Value::as_bool) == Some(true),
                }
            }
            Some(other) => return Err(UnsupportedSchema::new(&format!("type {other}"), &path)),
            None => return Err(UnsupportedSchema::new("property without a type", &path)),
        }
    };

    if matches!(&kind, FieldKind::Group { fields, .. } if fields.is_empty()) {
        return Err(UnsupportedSchema::new("object without properties", &path));
    }

    let widget = widget_for(node, &kind);
    let full_row = node.get("row").and_then(Value::as_bool) == Some(true)
        || matches!(kind, FieldKind::Group { .. })
        || matches!(widget, Widget::Textarea | Widget::Code);
    Ok(FieldModel {
        key: key.to_owned(),
        keys,
        path,
        title: node.get("title").and_then(Value::as_str).map(str::to_owned),
        description: node.get("description").and_then(Value::as_str).map(str::to_owned),
        default: node.get("default").cloned(),
        required,
        nullable,
        placeholder: non_blank_str(node, "placeholder").map(str::to_owned),
        help_url: help_url_of(node),
        widget,
        full_row,
        kind,
    })
}

fn build_properties(
    node: &Map<String, Value>,
    parent_keys: &[String],
    depth: usize,
) -> Result<Vec<FieldModel>, UnsupportedSchema> {
    let required: Vec<&str> = node
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let Some(properties) = node.get("properties").and_then(Value::as_object) else {
        return Ok(Vec::new());
    };
    properties
        .iter()
        .map(|(key, child)| build_field(key, parent_keys, child, required.contains(&key.as_str()), depth))
        .collect()
}

/// Checks that the form can render `schema` and builds the model it renders.
///
/// The first unsupported construct found is returned, so the form shows one
/// clear error instead of a partial form.
pub fn analyze_schema(schema: &Value) -> Result<FormModel, UnsupportedSchema> {
    if let Some(found) = find_unsupported_keyword(schema, "") {
        return Err(found);
    }
    let Some(root) = schema.as_object() else {
        return Err(UnsupportedSchema::new("schema that is not an object", ROOT_PATH));
    };
    match root.get("type").and_then(Value::as_str) {
        Some("object") => {}
        None if root.contains_key("properties") => {}
        _ => return Err(UnsupportedSchema::new("root schema that is not an object", ROOT_PATH)),
    }
    Ok(FormModel { fields: build_properties(root, &[], 0)?, columns: columns_of(root) })
}

/// Whether [`analyze_schema`] accepts the schema.
pub fn schema_supported(schema: &Value) -> bool { analyze_schema(schema).is_ok() }

// ---------------------------------------------------------------------------
// Values (pure)
// ---------------------------------------------------------------------------

/// The value at `keys`, if every level exists.
pub fn value_at<'a>(root: &'a Value, keys: &[String]) -> Option<&'a Value> {
    keys.iter().try_fold(root, |node, key| node.as_object()?.get(key))
}

/// A copy of `root` with the value at `keys` set, or removed for `None`.
pub fn set_value_at(root: &Value, keys: &[String], new_value: Option<Value>) -> Value {
    let mut result = if root.is_object() { root.clone() } else { Value::Object(Map::new()) };
    let mut node = &mut result;
    for (index, key) in keys.iter().enumerate() {
        let Some(object) = node.as_object_mut() else { break };
        if index + 1 == keys.len() {
            match &new_value {
                Some(value) => {
                    object.insert(key.clone(), value.clone());
                }
                None => {
                    // `shift_remove` keeps the other keys in place (`preserve_order`).
                    object.shift_remove(key);
                }
            }
            break;
        }
        let child = object.entry(key.clone()).or_insert_with(|| Value::Object(Map::new()));
        if !child.is_object() {
            *child = Value::Object(Map::new());
        }
        node = child;
    }
    result
}

fn is_absent(value: Option<&Value>) -> bool { matches!(value, None | Some(Value::Null)) }

fn prefill_fields(fields: &[FieldModel], object: &mut Map<String, Value>) {
    for field in fields {
        if let FieldKind::Group { fields: children, .. } = &field.kind {
            let mut group = match object.get(&field.key) {
                Some(Value::Object(existing)) => existing.clone(),
                _ => Map::new(),
            };
            prefill_fields(children, &mut group);
            if !group.is_empty() || object.get(&field.key).is_some_and(Value::is_object) {
                object.insert(field.key.clone(), Value::Object(group));
            }
        } else if field.kind != FieldKind::Secret && is_absent(object.get(&field.key)) {
            // A nullable field that was set to `null` on purpose keeps it.
            if field.nullable && object.get(&field.key).is_some_and(Value::is_null) {
                continue;
            }
            if let Some(default) = &field.default {
                object.insert(field.key.clone(), default.clone());
            }
        }
    }
}

/// `value` with every absent property filled from its schema `default`.
///
/// Secrets are never prefilled. A schema the form cannot render leaves the
/// value as it is.
pub fn prefill_defaults(schema: &Value, value: &Value) -> Value {
    let Ok(model) = analyze_schema(schema) else {
        return value.clone();
    };
    let mut object = value.as_object().cloned().unwrap_or_default();
    prefill_fields(&model.fields, &mut object);
    Value::Object(object)
}

/// Whether the secret at `path` is configured: the server's `secrets[]` flag,
/// else whether the value is a non-empty string.
pub fn secret_is_set(secrets: &[PluginSecretDto], path: &str, value: Option<&Value>) -> bool {
    secrets
        .iter()
        .find(|secret| secret.path == path)
        .map_or_else(|| value.and_then(Value::as_str).is_some_and(|text| !text.is_empty()), |secret| secret.set)
}

/// What the "new value" box of a secret shows: nothing while the value is the
/// one the form was opened with (the masked sentinel or an `${env:…}`
/// reference), else what the user typed.
pub fn secret_typed_text(current: Option<&Value>, original: Option<&Value>) -> String {
    if current == original {
        String::new()
    } else {
        current.and_then(Value::as_str).unwrap_or_default().to_owned()
    }
}

/// The value a secret takes after the user typed `typed`: an empty box restores the original value.
pub fn secret_after_input(typed: &str, original: Option<&Value>) -> Option<Value> {
    if typed.is_empty() {
        original.cloned()
    } else {
        Some(Value::String(typed.to_owned()))
    }
}

/// The text of a scalar JSON value, for tags and enum options.
pub fn scalar_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Messages of the errors whose path is `path` (or the elements of the list at `path`).
pub fn errors_for(errors: &[PluginFieldErrorDto], path: &str) -> Vec<String> {
    let items = item_path(path);
    errors.iter().filter(|error| error.path == path || error.path == items).map(|error| error.message.clone()).collect()
}

fn collect_paths<'a>(fields: &'a [FieldModel], paths: &mut Vec<&'a str>) {
    for field in fields {
        paths.push(&field.path);
        if let FieldKind::Group { fields: children, .. } = &field.kind {
            collect_paths(children, paths);
        }
    }
}

/// The errors that belong to no rendered field, such as `(root)`.
pub fn unmatched_errors<'a>(model: &FormModel, errors: &'a [PluginFieldErrorDto]) -> Vec<&'a PluginFieldErrorDto> {
    let mut paths = Vec::new();
    collect_paths(&model.fields, &mut paths);
    errors
        .iter()
        .filter(|error| !paths.iter().any(|path| error.path == *path || error.path == item_path(path)))
        .collect()
}

/// The unit a duration field is edited in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurationUnit {
    Seconds,
    Minutes,
    Hours,
    Days,
}

impl DurationUnit {
    pub const ALL: [Self; 4] = [Self::Seconds, Self::Minutes, Self::Hours, Self::Days];

    const fn seconds(self) -> u64 {
        match self {
            Self::Seconds => 1,
            Self::Minutes => 60,
            Self::Hours => 3_600,
            Self::Days => 86_400,
        }
    }

    const fn suffix(self) -> char {
        match self {
            Self::Seconds => 's',
            Self::Minutes => 'm',
            Self::Hours => 'h',
            Self::Days => 'd',
        }
    }

    const fn id(self) -> &'static str {
        match self {
            Self::Seconds => "s",
            Self::Minutes => "m",
            Self::Hours => "h",
            Self::Days => "d",
        }
    }

    fn from_id(id: &str) -> Option<Self> { Self::ALL.into_iter().find(|unit| unit.id() == id) }

    const fn label_key(self) -> &'static str {
        match self {
            Self::Seconds => "LABEL.PLUGIN_DURATION_SECONDS",
            Self::Minutes => "LABEL.PLUGIN_DURATION_MINUTES",
            Self::Hours => "LABEL.PLUGIN_DURATION_HOURS",
            Self::Days => "LABEL.PLUGIN_DURATION_DAYS",
        }
    }
}

/// How a duration field stores its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurationStyle {
    /// An integer number of seconds.
    Seconds,
    /// A string like `30m`.
    Compact,
    /// An ISO 8601 string like `PT30M` (`format: duration`).
    Iso,
}

impl DurationStyle {
    fn of(kind: &FieldKind) -> Self {
        match kind {
            FieldKind::Text { format: Some(format), .. } if format == "duration" => Self::Iso,
            FieldKind::Text { .. } => Self::Compact,
            _ => Self::Seconds,
        }
    }
}

/// A stored duration as an amount and the largest unit it is a whole number of.
pub fn duration_parts(value: &Value, style: DurationStyle) -> Option<(u64, DurationUnit)> {
    match style {
        DurationStyle::Seconds => {
            let seconds = value.as_u64()?;
            let unit = DurationUnit::ALL
                .into_iter()
                .rev()
                .find(|unit| seconds != 0 && seconds % unit.seconds() == 0)
                .unwrap_or(DurationUnit::Seconds);
            Some((seconds / unit.seconds(), unit))
        }
        DurationStyle::Compact => {
            let text = value.as_str()?.trim();
            let suffix = text.chars().last()?;
            let unit = DurationUnit::ALL.into_iter().find(|unit| unit.suffix() == suffix.to_ascii_lowercase())?;
            let amount = text[..text.len() - suffix.len_utf8()].trim().parse().ok()?;
            Some((amount, unit))
        }
        DurationStyle::Iso => {
            let text = value.as_str()?.trim().to_ascii_uppercase();
            let (body, unit) = if let Some(days) = text.strip_prefix('P').and_then(|rest| rest.strip_suffix('D')) {
                (days, DurationUnit::Days)
            } else {
                let rest = text.strip_prefix("PT")?;
                let unit = match rest.chars().last()? {
                    'S' => DurationUnit::Seconds,
                    'M' => DurationUnit::Minutes,
                    'H' => DurationUnit::Hours,
                    _ => return None,
                };
                (&rest[..rest.len() - 1], unit)
            };
            Some((body.parse().ok()?, unit))
        }
    }
}

/// The stored form of `amount` `unit`s.
pub fn duration_value(amount: u64, unit: DurationUnit, style: DurationStyle) -> Value {
    match style {
        DurationStyle::Seconds => Value::from(amount.saturating_mul(unit.seconds())),
        DurationStyle::Compact => Value::String(format!("{amount}{}", unit.suffix())),
        DurationStyle::Iso => Value::String(match unit {
            DurationUnit::Days => format!("P{amount}D"),
            other => format!("PT{amount}{}", other.suffix().to_ascii_uppercase()),
        }),
    }
}

/// The `type` of the text input for a string `format`.
fn input_type_for(format: Option<&str>) -> Option<String> {
    match format? {
        "email" => Some("email".to_owned()),
        "uri" => Some("url".to_owned()),
        "date" => Some("date".to_owned()),
        _ => None,
    }
}

/// What an empty field shows: `placeholder`, else an example of its format.
fn placeholder_for(field: &FieldModel) -> Option<String> {
    field.placeholder.clone().or_else(|| match &field.kind {
        FieldKind::Text { format: Some(format), .. } => match format.as_str() {
            "date-time" => Some("2026-01-31T18:00:00Z".to_owned()),
            "time" => Some("18:00:00Z".to_owned()),
            "duration" => Some("PT30M".to_owned()),
            "uri" => Some("https://".to_owned()),
            _ => None,
        },
        _ => None,
    })
}

fn field_dom_id(path: &str) -> String {
    let normalized: String = path.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    format!("plugin_setting_{normalized}")
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

#[derive(Properties, Clone, PartialEq)]
pub struct SchemaFormProps {
    /// The settings JSON Schema.
    pub schema: Value,
    /// The edited settings object.
    pub value: Value,
    /// Which secret properties are configured.
    #[prop_or_default]
    pub secrets: Vec<PluginSecretDto>,
    /// Violations of the last save, shown under the matching fields.
    #[prop_or_default]
    pub errors: Vec<PluginFieldErrorDto>,
    /// The whole edited object after every change.
    pub on_change: Callback<Value>,
}

/// Everything a field needs to render and to report a change.
struct FormContext<'a> {
    value: &'a Value,
    secrets: &'a [PluginSecretDto],
    errors: &'a [PluginFieldErrorDto],
    on_change: &'a Callback<Value>,
}

impl FormContext<'_> {
    /// A callback that stores a field's new value (or removes it) and emits the object.
    fn setter(&self, field: &FieldModel) -> Callback<Option<Value>> {
        let root = self.value.clone();
        let keys = field.keys.clone();
        let on_change = self.on_change.clone();
        Callback::from(move |new_value: Option<Value>| on_change.emit(set_value_at(&root, &keys, new_value)))
    }

    /// The value shown for a field: the stored one, else its default.
    fn effective<'v>(&'v self, field: &'v FieldModel) -> Option<&'v Value> {
        value_at(self.value, &field.keys).filter(|v| !v.is_null()).or(field.default.as_ref())
    }
}

fn render_errors(messages: &[String]) -> Html {
    html! {
        <>{ for messages.iter().map(|message| html! { <span class="tp__input-error" role="alert">{ message.clone() }</span> }) }</>
    }
}

fn joined_error(errors: &[String]) -> Option<String> { (!errors.is_empty()).then(|| errors.join(" ")) }

#[derive(Properties, Clone, PartialEq)]
struct SchemaFieldLabelProps {
    label: String,
    field_id: String,
    description: Option<String>,
    #[prop_or_default]
    help_url: Option<String>,
}

/// The label of a field. A plugin's `description` is free text, not an i18n
/// key, so it is not routed through `FieldLabel`: the help button opens it in
/// the same explanation dialog instead.
#[component]
fn SchemaFieldLabel(props: &SchemaFieldLabelProps) -> Html {
    let dialog = use_context::<DialogService>();
    let translate = use_translation();

    let on_help = {
        let dialog = dialog.clone();
        let title = props.label.clone();
        let description = props.description.clone();
        Callback::from(move |(_name, _e): (String, MouseEvent)| {
            if let (Some(dialog), Some(description)) = (dialog.as_ref(), description.as_ref()) {
                let actions = DialogActions {
                    left: None,
                    right: vec![DialogAction::new_focused(
                        "close",
                        "LABEL.CLOSE",
                        DialogResult::Cancel,
                        Some("Close".to_owned()),
                        None,
                    )],
                };
                let content = html! {
                    <div class="tp__field-explanation-dialog">
                        <div class="tp__field-explanation-dialog__header"><h2>{ title.clone() }</h2></div>
                        <div class="tp__field-explanation-dialog__body"><p>{ description.clone() }</p></div>
                    </div>
                };
                let dialog = dialog.clone();
                yew::platform::spawn_local(async move {
                    let _ = dialog.content(content, Some(actions), true).await;
                });
            }
        })
    };

    html! {
        <div class="tp__field-label">
            <label for={props.field_id.clone()}>{ props.label.clone() }</label>
            if props.description.is_some() && dialog.is_some() {
                <IconButton
                    name="help"
                    icon="QuestionMark"
                    class="tp__field-label__help"
                    hint={translate.t("LABEL.HELP")}
                    onclick={on_help}
                />
            }
            if let Some(url) = props.help_url.clone() {
                <a class="tp__icon-button tp__field-label__help tp__schema-form__doc-link" href={url}
                    target="_blank" rel="noopener noreferrer" title={translate.t("LABEL.DOCUMENTATION")}>
                    <AppIcon name="Link"/>
                </a>
            }
        </div>
    }
}

fn field_label(field: &FieldModel, field_id: &str) -> Html {
    html! {
        <SchemaFieldLabel label={field.label().to_owned()} field_id={field_id.to_owned()}
            description={field.description.clone()} help_url={field.help_url.clone()} />
    }
}

#[derive(Properties, Clone, PartialEq)]
struct SecretFieldProps {
    field: Rc<FieldModel>,
    value: Option<Value>,
    is_set: bool,
    errors: Vec<String>,
    on_change: Callback<Option<Value>>,
}

/// A secret keeps the value it came with (the masked sentinel or an
/// `${env:…}` reference) unless the user types a new one, so a save
/// round-trips without the form ever holding the stored secret.
#[component]
fn SecretField(props: &SecretFieldProps) -> Html {
    let translate = use_translation();
    // The value the form was opened with; the form is remounted after a save.
    let original = use_state(|| props.value.clone());
    let field_id = field_dom_id(&props.field.path);
    let typed = secret_typed_text(props.value.as_ref(), (*original).as_ref());

    let on_input = {
        let original = original.clone();
        let on_change = props.on_change.clone();
        Callback::from(move |typed: String| on_change.emit(secret_after_input(&typed, (*original).as_ref())))
    };

    let shown = props.value.as_ref().map(scalar_text).filter(|text| !text.is_empty());
    let state_label = if props.is_set { "LABEL.PLUGIN_SECRET_SET" } else { "LABEL.PLUGIN_SECRET_NOT_SET" };

    html! {
        <div class="tp__schema-form__field tp__schema-form__secret">
            { field_label(&props.field, &field_id) }
            <div class="tp__schema-form__secret-state">
                <span class="tp__schema-form__secret-badge">{ translate.t(state_label) }</span>
                if let Some(shown) = shown {
                    <HideContent content={html! { { shown } }} />
                }
            </div>
            <Input
                name={props.field.path.clone()}
                field_id={Some(field_id)}
                hidden={true}
                value={typed}
                placeholder={Some(translate.t("LABEL.PLUGIN_SECRET_NEW_VALUE"))}
                on_change={Some(on_input)}
                error={joined_error(&props.errors)}
            />
            <small class="tp__schema-form__hint">{ translate.t("EXPLANATION.PLUGIN_SECRET_ENV_HINT") }</small>
        </div>
    }
}

#[derive(Properties, Clone, PartialEq)]
struct DurationFieldProps {
    field: Rc<FieldModel>,
    value: Option<Value>,
    errors: Vec<String>,
    on_change: Callback<Option<Value>>,
}

/// An amount and a unit. The value is stored as [`DurationStyle`] says.
#[component]
fn DurationField(props: &DurationFieldProps) -> Html {
    let translate = use_translation();
    let style = DurationStyle::of(&props.field.kind);
    let parts = props.value.as_ref().and_then(|value| duration_parts(value, style));
    // The unit is remembered while the amount is empty, so picking a unit first works.
    let unit = use_state(|| parts.map_or(DurationUnit::Minutes, |(_, unit)| unit));
    let current_unit = parts.map_or(*unit, |(_, unit)| unit);
    let amount = parts.map(|(amount, _)| amount);
    let field_id = field_dom_id(&props.field.path);

    let on_amount = {
        let on_change = props.on_change.clone();
        Callback::from(move |number: Option<i64>| {
            let amount = number.and_then(|number| u64::try_from(number).ok());
            on_change.emit(amount.map(|amount| duration_value(amount, current_unit, style)));
        })
    };
    let options = Rc::new(
        DurationUnit::ALL
            .into_iter()
            .map(|option| {
                DropDownOption::new(option.id(), html! { { translate.t(option.label_key()) } }, option == current_unit)
            })
            .collect::<Vec<_>>(),
    );
    let on_unit = {
        let unit = unit.clone();
        let on_change = props.on_change.clone();
        Callback::from(move |(_, selection): (String, DropDownSelection)| {
            if let DropDownSelection::Single(id) = selection {
                if let Some(next) = DurationUnit::from_id(&id) {
                    unit.set(next);
                    if let Some(amount) = amount {
                        on_change.emit(Some(duration_value(amount, next, style)));
                    }
                }
            }
        })
    };

    html! {
        <div class="tp__schema-form__field">
            { field_label(&props.field, &field_id) }
            <div class="tp__schema-form__duration">
                <NumberInput name={props.field.path.clone()} field_id={Some(field_id.clone())}
                    value={amount.and_then(|amount| i64::try_from(amount).ok())}
                    placeholder={props.field.placeholder.clone().unwrap_or_default()}
                    on_change={on_amount} />
                <Select name={format!("{field_id}_unit")} options={options} on_select={on_unit} />
            </div>
            { render_errors(&props.errors) }
        </div>
    }
}

fn render_enum(
    ctx: &FormContext<'_>,
    field: &FieldModel,
    values: &[Value],
    labels: &[String],
    errors: &[String],
) -> Html {
    let field_id = field_dom_id(&field.path);
    let current = ctx.effective(field).map(scalar_text);
    let setter = ctx.setter(field);
    let choices = values.to_vec();
    let pick = move |id: &str| {
        if let Some(choice) = choices.iter().find(|choice| scalar_text(choice) == id) {
            setter.emit(Some(choice.clone()));
        }
    };

    if field.widget == Widget::Radio {
        let options = Rc::new(values.iter().map(scalar_text).collect::<Vec<_>>());
        let selected = Rc::new(current.into_iter().collect::<Vec<_>>());
        let on_select = Callback::from(move |picked: Rc<Vec<String>>| {
            if let Some(id) = picked.first() {
                pick(id);
            }
        });
        return html! {
            <div class="tp__schema-form__field">
                { field_label(field, &field_id) }
                <RadioButtonGroup options={options} selected={selected} labels={Some(Rc::new(labels.to_vec()))}
                    on_select={on_select} wrap={true} />
                { render_errors(errors) }
            </div>
        };
    }

    let options = Rc::new(
        values
            .iter()
            .zip(labels)
            .map(|(option, label)| {
                let text = scalar_text(option);
                let selected = current.as_deref() == Some(text.as_str());
                DropDownOption::new(&text, html! { { label.clone() } }, selected)
            })
            .collect::<Vec<_>>(),
    );
    let on_select = Callback::from(move |(_, selection): (String, DropDownSelection)| {
        if let DropDownSelection::Single(id) = selection {
            pick(&id);
        }
    });
    html! {
        <div class="tp__schema-form__field">
            { field_label(field, &field_id) }
            <Select name={field_id} options={options} on_select={on_select} required={field.required}
                error={joined_error(errors)} />
        </div>
    }
}

fn render_multi_select(
    ctx: &FormContext<'_>,
    field: &FieldModel,
    values: &[Value],
    labels: &[String],
    errors: &[String],
) -> Html {
    let field_id = field_dom_id(&field.path);
    let current: Vec<String> = ctx
        .effective(field)
        .and_then(Value::as_array)
        .map(|items| items.iter().map(scalar_text).collect())
        .unwrap_or_default();
    let options = Rc::new(
        values
            .iter()
            .zip(labels)
            .map(|(option, label)| {
                let text = scalar_text(option);
                let selected = current.contains(&text);
                DropDownOption::new(&text, html! { { label.clone() } }, selected)
            })
            .collect::<Vec<_>>(),
    );
    let setter = ctx.setter(field);
    let choices = values.to_vec();
    let required = field.required;
    let on_select = Callback::from(move |(_, selection): (String, DropDownSelection)| {
        let ids = match selection {
            DropDownSelection::Empty => Vec::new(),
            DropDownSelection::Single(id) => vec![id],
            DropDownSelection::Multi(ids) => ids,
        };
        // Kept in the order of `enum`, whatever order they were clicked in.
        let picked: Vec<Value> = choices.iter().filter(|choice| ids.contains(&scalar_text(choice))).cloned().collect();
        setter.emit(if picked.is_empty() && !required { None } else { Some(Value::Array(picked)) });
    });
    html! {
        <div class="tp__schema-form__field">
            { field_label(field, &field_id) }
            <Select name={field_id} options={options} on_select={on_select} multi_select={true}
                required={field.required} error={joined_error(errors)} />
        </div>
    }
}

fn render_list(
    ctx: &FormContext<'_>,
    field: &FieldModel,
    element: ScalarKind,
    (unique, max_items): (bool, Option<usize>),
    errors: &[String],
) -> Html {
    let field_id = field_dom_id(&field.path);
    let tags: Vec<Rc<Tag>> = ctx
        .effective(field)
        .and_then(Value::as_array)
        .map(|items| items.iter().map(|item| Rc::new(Tag { label: scalar_text(item), class: None })).collect())
        .unwrap_or_default();
    let setter = ctx.setter(field);
    let required = field.required;
    let on_change = Callback::from(move |tags: Vec<Rc<Tag>>| {
        let items = tags.iter().filter_map(|tag| element.parse(&tag.label)).collect::<Vec<_>>();
        setter.emit(if items.is_empty() && !required { None } else { Some(Value::Array(items)) });
    });
    let existing: Vec<String> = tags.iter().map(|tag| tag.label.clone()).collect();
    let create_tag = Callback::from(move |raw: String| {
        let label = raw.trim().to_owned();
        if max_items.is_some_and(|max| existing.len() >= max) || (unique && existing.contains(&label)) {
            return None;
        }
        element.parse(&raw).map(|_| Tag { label, class: None })
    });
    html! {
        <div class="tp__schema-form__field">
            { field_label(field, &field_id) }
            <TagList tags={tags} readonly={false} on_change={on_change} create_tag={create_tag}
                placeholder={field.placeholder.clone().unwrap_or_else(|| "Add tag...".to_owned())} />
            { render_errors(errors) }
        </div>
    }
}

/// A range input over `[min, max]`, with the current value beside it.
fn render_slider(ctx: &FormContext<'_>, field: &FieldModel, errors: &[String]) -> Html {
    let field_id = field_dom_id(&field.path);
    let setter = ctx.setter(field);
    let current = ctx.effective(field).and_then(Value::as_f64);
    let (min, max, step, integer) = match &field.kind {
        FieldKind::Integer { min, max, step } => (
            min.map(|min| min as f64),
            max.map(|max| max as f64),
            step.map_or_else(|| "1".to_owned(), |step| step.to_string()),
            true,
        ),
        FieldKind::Number { min, max, step } => {
            (*min, *max, step.map_or_else(|| "any".to_owned(), |step| step.to_string()), false)
        }
        _ => (None, None, "any".to_owned(), false),
    };
    let shown = current.or(min).unwrap_or_default();
    let oninput = Callback::from(move |event: InputEvent| {
        let input: HtmlInputElement = event.target_unchecked_into();
        let Ok(number) = input.value().parse::<f64>() else { return };
        setter.emit(if integer {
            Some(Value::from(number.round() as i64))
        } else {
            Number::from_f64(number).map(Value::Number)
        });
    });
    let text = if integer { format!("{}", shown.round() as i64) } else { format!("{shown}") };
    html! {
        <div class="tp__schema-form__field">
            { field_label(field, &field_id) }
            <div class="tp__schema-form__slider">
                <input type="range" id={field_id} name={field.path.clone()}
                    min={min.map(|min| min.to_string())} max={max.map(|max| max.to_string())} step={step}
                    value={shown.to_string()} oninput={oninput} />
                <span class="tp__schema-form__slider-value">{ text }</span>
            </div>
            { render_errors(errors) }
        </div>
    }
}

fn render_integer(
    ctx: &FormContext<'_>,
    field: &FieldModel,
    bounds: (Option<i64>, Option<i64>),
    errors: &[String],
) -> Html {
    let field_id = field_dom_id(&field.path);
    let setter = ctx.setter(field);
    let current = ctx.effective(field).and_then(Value::as_i64);
    html! {
        <div class="tp__schema-form__field">
            { field_label(field, &field_id) }
            <NumberInput name={field.path.clone()} field_id={Some(field_id)} value={current}
                min_i64={bounds.0} max_i64={bounds.1}
                placeholder={field.placeholder.clone().unwrap_or_default()}
                on_change={Callback::from(move |number: Option<i64>| setter.emit(number.map(Value::from)))} />
            { render_errors(errors) }
        </div>
    }
}

fn render_number(ctx: &FormContext<'_>, field: &FieldModel, errors: &[String]) -> Html {
    let field_id = field_dom_id(&field.path);
    let setter = ctx.setter(field);
    let current = ctx.effective(field).and_then(Value::as_f64);
    html! {
        <div class="tp__schema-form__field">
            { field_label(field, &field_id) }
            <NumberInput name={field.path.clone()} field_id={Some(field_id)} float_value={current}
                placeholder={field.placeholder.clone().unwrap_or_default()}
                on_change_float={Some(Callback::from(move |number: Option<f64>| {
                    setter.emit(number.and_then(Number::from_f64).map(Value::Number));
                }))} />
            { render_errors(errors) }
        </div>
    }
}

fn render_text(ctx: &FormContext<'_>, field: &FieldModel, errors: &[String]) -> Html {
    let field_id = field_dom_id(&field.path);
    let setter = ctx.setter(field);
    let required = field.required;
    let current = ctx.effective(field).map(scalar_text).unwrap_or_default();
    let on_text = Callback::from(move |text: String| {
        setter.emit(if text.is_empty() && !required { None } else { Some(Value::String(text)) });
    });
    let (format, max_length) = match &field.kind {
        FieldKind::Text { format, max_length } => (format.as_deref(), *max_length),
        _ => (None, None),
    };
    match field.widget {
        Widget::Textarea | Widget::Code => html! {
            <div class={classes!("tp__schema-form__field", (field.widget == Widget::Code).then_some("tp__schema-form__code"))}>
                { field_label(field, &field_id) }
                <TextArea name={field.path.clone()} field_id={Some(field_id)} value={current}
                    rows={Some(if field.widget == Widget::Code { 8 } else { 4 })}
                    placeholder={placeholder_for(field)} on_change={Some(on_text)} />
                { render_errors(errors) }
            </div>
        },
        _ => {
            let input_type =
                if field.widget == Widget::Color { Some("color".to_owned()) } else { input_type_for(format) };
            html! {
                <div class={classes!("tp__schema-form__field", (field.widget == Widget::Color).then_some("tp__schema-form__color"))}>
                    { field_label(field, &field_id) }
                    <Input name={field.path.clone()} field_id={Some(field_id)} value={current} required={required}
                        input_type={input_type} max_length={max_length} placeholder={placeholder_for(field)}
                        on_change={Some(on_text)} error={joined_error(errors)} />
                </div>
            }
        }
    }
}

/// `fields` in `columns` columns; a single column renders them one under another.
fn render_fields(ctx: &FormContext<'_>, fields: &[FieldModel], columns: u8) -> Html {
    if columns <= 1 {
        return html! { <>{ for fields.iter().map(|field| render_field(ctx, field)) }</> };
    }
    html! {
        <div class="tp__schema-form__grid" style={format!("--tp-schema-form-columns: {columns}")}>
            { for fields.iter().map(|field| html! {
                <div class={classes!("tp__schema-form__cell", field.full_row.then_some("tp__schema-form__cell--row"))}>
                    { render_field(ctx, field) }
                </div>
            }) }
        </div>
    }
}

fn render_field(ctx: &FormContext<'_>, field: &FieldModel) -> Html {
    let errors = errors_for(ctx.errors, &field.path);
    if field.widget == Widget::Duration {
        return html! {
            <DurationField key={field.path.clone()} field={Rc::new(field.clone())}
                value={value_at(ctx.value, &field.keys).filter(|v| !v.is_null()).or(field.default.as_ref()).cloned()}
                errors={errors} on_change={ctx.setter(field)} />
        };
    }
    if field.widget == Widget::Slider {
        return render_slider(ctx, field, &errors);
    }
    match &field.kind {
        FieldKind::Text { .. } => render_text(ctx, field, &errors),
        FieldKind::Secret => {
            let value = value_at(ctx.value, &field.keys).cloned();
            let is_set = secret_is_set(ctx.secrets, &field.path, value.as_ref());
            html! {
                <SecretField key={field.path.clone()} field={Rc::new(field.clone())} value={value} is_set={is_set}
                    errors={errors} on_change={ctx.setter(field)} />
            }
        }
        FieldKind::Integer { min, max, .. } => render_integer(ctx, field, (*min, *max), &errors),
        FieldKind::Number { .. } => render_number(ctx, field, &errors),
        FieldKind::Boolean => {
            let setter = ctx.setter(field);
            let current = ctx.effective(field).and_then(Value::as_bool).unwrap_or(false);
            html! {
                <div class="tp__schema-form__field tp__schema-form__field--inline">
                    { field_label(field, &field_dom_id(&field.path)) }
                    <ToggleSwitch value={current} aria_label={Some(field.label().to_owned())}
                        on_change={Callback::from(move |on: bool| setter.emit(Some(Value::Bool(on))))} />
                    { render_errors(&errors) }
                </div>
            }
        }
        FieldKind::Enum { values, labels } => render_enum(ctx, field, values, labels, &errors),
        FieldKind::MultiSelect { values, labels } => render_multi_select(ctx, field, values, labels, &errors),
        FieldKind::List { element, unique, max_items } => {
            render_list(ctx, field, *element, (*unique, *max_items), &errors)
        }
        FieldKind::Group { fields, columns, collapsed } => html! {
            <Accordion default_panel={(!collapsed).then(|| field.path.clone())}>
                <AccordionPanel id={field.path.clone()} title={field.label().to_owned()}>
                    if let Some(description) = field.description.as_ref() {
                        <p class="tp__schema-form__hint">{ description.clone() }</p>
                    }
                    { render_errors(&errors) }
                    { render_fields(ctx, fields, *columns) }
                </AccordionPanel>
            </Accordion>
        },
    }
}

#[component]
pub fn SchemaForm(props: &SchemaFormProps) -> Html {
    let translate = use_translation();
    let model = analyze_schema(&props.schema);

    match model {
        Err(unsupported) => html! {
            <div class="tp__schema-form__unsupported tp__input-error" role="alert">
                { format!("{} {}", translate.t("MESSAGES.PLUGIN_SCHEMA_UNSUPPORTED"), unsupported.describe()) }
            </div>
        },
        Ok(model) => {
            let ctx = FormContext {
                value: &props.value,
                secrets: &props.secrets,
                errors: &props.errors,
                on_change: &props.on_change,
            };
            let loose = unmatched_errors(&model, &props.errors);
            html! {
                <div class="tp__schema-form">
                    for error in loose {
                        <span class="tp__input-error" role="alert">
                            { format!("{}: {}", error.path, error.message) }
                        </span>
                    }
                    { render_fields(&ctx, &model.fields, model.columns) }
                </div>
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn field<'a>(model: &'a FormModel, key: &str) -> &'a FieldModel {
        model.fields.iter().find(|field| field.key == key).expect("field")
    }

    fn unsupported(schema: &Value) -> String { analyze_schema(schema).expect_err("unsupported").construct }

    fn group_fields(field: &FieldModel) -> &[FieldModel] {
        let FieldKind::Group { fields, .. } = &field.kind else { panic!("group") };
        fields
    }

    #[test]
    fn supported_subset_builds_a_model() {
        let schema = json!({
            "type": "object",
            "required": ["host"],
            "properties": {
                "host": {"type": "string", "title": "Host", "description": "Server host"},
                "port": {"type": "integer", "minimum": 1, "maximum": 65535},
                "ratio": {"type": "number"},
                "verbose": {"type": "boolean"},
                "mode": {"enum": ["fast", "slow"]},
                "tags": {"type": "array", "items": {"type": "string"}},
                "auth": {"type": "object", "properties": {"user": {"type": "string"}}}
            }
        });
        let model = analyze_schema(&schema).expect("supported");
        assert_eq!(model.fields.len(), 7);
        assert_eq!(field(&model, "host").label(), "Host");
        assert!(field(&model, "host").required);
        assert!(!field(&model, "port").required);
        assert_eq!(field(&model, "port").kind, FieldKind::Integer { min: Some(1), max: Some(65535), step: None });
        assert_eq!(field(&model, "ratio").kind, FieldKind::Number { min: None, max: None, step: None });
        assert_eq!(field(&model, "verbose").kind, FieldKind::Boolean);
        assert_eq!(
            field(&model, "mode").kind,
            FieldKind::Enum { values: vec![json!("fast"), json!("slow")], labels: vec!["fast".into(), "slow".into()] }
        );
        assert_eq!(
            field(&model, "tags").kind,
            FieldKind::List { element: ScalarKind::Text, unique: false, max_items: None }
        );
        assert_eq!(field(&model, "tags").label(), "tags");
        assert!(matches!(field(&model, "auth").kind, FieldKind::Group { columns: 1, collapsed: false, .. }));
        assert!(schema_supported(&schema));
        assert_eq!(model.columns, 1);
    }

    #[test]
    fn fields_keep_the_order_of_the_schema() {
        let schema: Value =
            serde_json::from_str(r#"{"type":"object","properties":{"zeta":{"type":"string"},"alpha":{"type":"string"},"mid":{"type":"string"}}}"#)
                .unwrap();
        let model = analyze_schema(&schema).expect("supported");
        let keys: Vec<&str> = model.fields.iter().map(|field| field.key.as_str()).collect();
        assert_eq!(keys, ["zeta", "alpha", "mid"]);
    }

    #[test]
    fn every_unsupported_keyword_is_named() {
        for keyword in UNSUPPORTED_KEYWORDS {
            let schema = json!({"type": "object", "properties": {"a": {"type": "string", keyword: {}}}});
            assert_eq!(unsupported(&schema), keyword, "{keyword}");
        }
    }

    #[test]
    fn unsupported_keywords_are_found_at_the_root_and_deep_down() {
        let root = json!({"type": "object", "oneOf": [], "properties": {}});
        let found = analyze_schema(&root).expect_err("oneOf");
        assert_eq!((found.construct.as_str(), found.path.as_str()), ("oneOf", ROOT_PATH));

        let deep = json!({"type": "object", "properties": {
            "group": {"type": "object", "properties": {"x": {"type": "string", "allOf": []}}}}});
        let found = analyze_schema(&deep).expect_err("allOf");
        assert_eq!((found.construct.as_str(), found.path.as_str()), ("allOf", "group.x"));

        let in_items = json!({"type": "object", "properties": {"l": {"type": "array", "items": {"$ref": "#/x"}}}});
        let found = analyze_schema(&in_items).expect_err("$ref");
        assert_eq!((found.construct.as_str(), found.path.as_str()), ("$ref", "l[]"));
    }

    #[test]
    fn a_property_named_like_a_keyword_is_not_a_keyword() {
        let schema = json!({"type": "object", "properties": {"not": {"type": "string"}, "if": {"type": "boolean"}}});
        assert!(analyze_schema(&schema).is_ok());
    }

    #[test]
    fn objects_nest_several_levels_deep() {
        let schema = json!({"type": "object", "properties": {
            "a": {"type": "object", "properties": {"b": {"type": "object", "properties": {"c": {"type": "string", "default": "x"}}}}}}});
        let model = analyze_schema(&schema).expect("supported");
        let b = &group_fields(field(&model, "a"))[0];
        let c = &group_fields(b)[0];
        assert_eq!(c.path, "a.b.c");
        assert_eq!(c.keys, ["a", "b", "c"]);
        assert_eq!(prefill_defaults(&schema, &json!({})), json!({"a": {"b": {"c": "x"}}}));
    }

    #[test]
    fn arrays_of_objects_and_arrays_are_unsupported() {
        let objects = json!({"type": "object", "properties": {
            "servers": {"type": "array", "items": {"type": "object", "properties": {"k": {"type": "string"}}}}}});
        assert_eq!(unsupported(&objects), "array of objects");
        let nested = json!({"type": "object", "properties": {"m": {"type": "array", "items": {"type": "array"}}}});
        assert_eq!(unsupported(&nested), "array of arrays");
    }

    #[test]
    fn odd_types_and_roots_are_unsupported() {
        assert_eq!(unsupported(&json!({"type": "string"})), "root schema that is not an object");
        assert_eq!(unsupported(&json!(true)), "schema that is not an object");
        let several = json!({"type": "object", "properties": {"a": {"type": ["string", "integer"]}}});
        assert_eq!(unsupported(&several), "type given as a list of several types");
        let untyped = json!({"type": "object", "properties": {"a": {}}});
        assert_eq!(unsupported(&untyped), "property without a type");
        let null_type = json!({"type": "object", "properties": {"a": {"type": "null"}}});
        assert_eq!(unsupported(&null_type), "type null");
    }

    #[test]
    fn a_type_list_with_null_is_a_nullable_field() {
        let schema = json!({"type": "object", "properties": {
            "name": {"type": ["string", "null"], "default": "x"},
            "count": {"type": ["null", "integer"]}}});
        let model = analyze_schema(&schema).expect("supported");
        assert!(field(&model, "name").nullable);
        assert!(matches!(field(&model, "name").kind, FieldKind::Text { .. }));
        assert!(matches!(field(&model, "count").kind, FieldKind::Integer { .. }));
        // An explicit null is a choice, not a missing value.
        assert_eq!(prefill_defaults(&schema, &json!({"name": null})), json!({"name": null}));
        assert_eq!(prefill_defaults(&schema, &json!({})), json!({"name": "x"}));
    }

    #[test]
    fn string_formats_lengths_and_bounds_reach_the_model() {
        let schema = json!({"type": "object", "properties": {
            "mail": {"type": "string", "format": "email", "maxLength": 80},
            "count": {"type": "integer", "exclusiveMinimum": 0, "exclusiveMaximum": 10, "multipleOf": 2},
            "ratio": {"type": "number", "minimum": 0.5, "maximum": 2.5, "multipleOf": 0.5}}});
        let model = analyze_schema(&schema).expect("supported");
        assert_eq!(field(&model, "mail").kind, FieldKind::Text { format: Some("email".into()), max_length: Some(80) });
        assert_eq!(field(&model, "count").kind, FieldKind::Integer { min: Some(1), max: Some(9), step: Some(2) });
        assert_eq!(field(&model, "ratio").kind, FieldKind::Number { min: Some(0.5), max: Some(2.5), step: Some(0.5) });
        assert_eq!(input_type_for(Some("email")).as_deref(), Some("email"));
        assert_eq!(input_type_for(Some("uri")).as_deref(), Some("url"));
        assert_eq!(input_type_for(Some("hostname")), None);
    }

    #[test]
    fn widgets_are_kept_only_where_they_fit() {
        let schema = json!({"type": "object", "properties": {
            "notes": {"type": "string", "widget": "textarea"},
            "script": {"type": "string", "widget": "code"},
            "tint": {"type": "string", "widget": "color"},
            "level": {"type": "integer", "minimum": 0, "maximum": 10, "widget": "slider"},
            "open": {"type": "integer", "widget": "slider"},
            "mode": {"enum": ["a", "b"], "widget": "radio"},
            "wait": {"type": "integer", "widget": "duration"},
            "flag": {"type": "boolean", "widget": "textarea"}}});
        let model = analyze_schema(&schema).expect("supported");
        assert_eq!(field(&model, "notes").widget, Widget::Textarea);
        assert!(field(&model, "notes").full_row);
        assert_eq!(field(&model, "script").widget, Widget::Code);
        assert_eq!(field(&model, "tint").widget, Widget::Color);
        assert_eq!(field(&model, "level").widget, Widget::Slider);
        assert_eq!(field(&model, "open").widget, Widget::Default, "a slider needs both bounds");
        assert_eq!(field(&model, "mode").widget, Widget::Radio);
        assert_eq!(field(&model, "wait").widget, Widget::Duration);
        assert_eq!(field(&model, "flag").widget, Widget::Default);
    }

    #[test]
    fn enum_labels_come_from_a_list_or_a_map() {
        let schema = json!({"type": "object", "properties": {
            "list": {"enum": ["live_only", "all"], "enumLabels": ["Live only"]},
            "map": {"enum": [1, 2], "enumLabels": {"2": "Two"}},
            "pick": {"type": "array", "items": {"enum": ["a", "b"], "enumLabels": ["A", "B"]}}}});
        let model = analyze_schema(&schema).expect("supported");
        let FieldKind::Enum { labels, .. } = &field(&model, "list").kind else { panic!("enum") };
        assert_eq!(labels, &["Live only", "all"]);
        let FieldKind::Enum { labels, .. } = &field(&model, "map").kind else { panic!("enum") };
        assert_eq!(labels, &["1", "Two"]);
        assert_eq!(
            field(&model, "pick").kind,
            FieldKind::MultiSelect { values: vec![json!("a"), json!("b")], labels: vec!["A".into(), "B".into()] }
        );
    }

    #[test]
    fn list_options_unique_and_max_items_reach_the_model() {
        let schema = json!({"type": "object", "properties": {
            "tags": {"type": "array", "items": {"type": "integer"}, "uniqueItems": true, "maxItems": 3}}});
        let model = analyze_schema(&schema).expect("supported");
        assert_eq!(
            field(&model, "tags").kind,
            FieldKind::List { element: ScalarKind::Integer, unique: true, max_items: Some(3) }
        );
    }

    #[test]
    fn layout_hints_reach_the_model() {
        let schema = json!({"type": "object", "columns": 2, "properties": {
            "a": {"type": "string", "placeholder": "e.g. abc", "helpUrl": "https://docs.example/a"},
            "b": {"type": "string", "row": true, "helpUrl": "javascript:alert(1)"},
            "g": {"type": "object", "columns": 9, "collapsed": true, "properties": {"c": {"type": "string"}}}}});
        let model = analyze_schema(&schema).expect("supported");
        assert_eq!(model.columns, 2);
        let a = field(&model, "a");
        assert_eq!(a.placeholder.as_deref(), Some("e.g. abc"));
        assert_eq!(a.help_url.as_deref(), Some("https://docs.example/a"));
        assert!(!a.full_row);
        let b = field(&model, "b");
        assert!(b.full_row);
        assert_eq!(b.help_url, None, "only http(s) links are kept");
        let g = field(&model, "g");
        assert!(g.full_row);
        assert!(
            matches!(g.kind, FieldKind::Group { columns: 1, collapsed: true, .. }),
            "out-of-range columns fall back to 1"
        );
    }

    #[test]
    fn durations_round_trip_in_every_style() {
        assert_eq!(duration_parts(&json!(5400), DurationStyle::Seconds), Some((90, DurationUnit::Minutes)));
        assert_eq!(duration_parts(&json!(7200), DurationStyle::Seconds), Some((2, DurationUnit::Hours)));
        assert_eq!(duration_parts(&json!(0), DurationStyle::Seconds), Some((0, DurationUnit::Seconds)));
        assert_eq!(duration_value(90, DurationUnit::Minutes, DurationStyle::Seconds), json!(5400));

        assert_eq!(duration_parts(&json!("30m"), DurationStyle::Compact), Some((30, DurationUnit::Minutes)));
        assert_eq!(duration_parts(&json!("2D"), DurationStyle::Compact), Some((2, DurationUnit::Days)));
        assert_eq!(duration_parts(&json!("soon"), DurationStyle::Compact), None);
        assert_eq!(duration_value(12, DurationUnit::Hours, DurationStyle::Compact), json!("12h"));

        assert_eq!(duration_parts(&json!("PT30M"), DurationStyle::Iso), Some((30, DurationUnit::Minutes)));
        assert_eq!(duration_parts(&json!("P3D"), DurationStyle::Iso), Some((3, DurationUnit::Days)));
        assert_eq!(duration_parts(&json!("P1DT2H"), DurationStyle::Iso), None);
        assert_eq!(duration_value(3, DurationUnit::Days, DurationStyle::Iso), json!("P3D"));
        assert_eq!(duration_value(45, DurationUnit::Seconds, DurationStyle::Iso), json!("PT45S"));

        let iso = FieldKind::Text { format: Some("duration".into()), max_length: None };
        assert_eq!(DurationStyle::of(&iso), DurationStyle::Iso);
        assert_eq!(DurationStyle::of(&FieldKind::Text { format: None, max_length: None }), DurationStyle::Compact);
        assert_eq!(DurationStyle::of(&FieldKind::Integer { min: None, max: None, step: None }), DurationStyle::Seconds);
    }

    #[test]
    fn secrets_are_detected_by_write_only_or_password_format() {
        let schema = json!({"type": "object", "properties": {
            "token": {"type": "string", "writeOnly": true},
            "key": {"type": "string", "format": "password"},
            "plain": {"type": "string", "writeOnly": false},
            "name": {"type": "string"}}});
        let model = analyze_schema(&schema).expect("supported");
        assert_eq!(field(&model, "token").kind, FieldKind::Secret);
        assert_eq!(field(&model, "key").kind, FieldKind::Secret);
        assert!(matches!(field(&model, "plain").kind, FieldKind::Text { .. }));
        assert!(matches!(field(&model, "name").kind, FieldKind::Text { .. }));
    }

    #[test]
    fn secret_inside_a_group_is_detected_with_its_path() {
        let schema = json!({"type": "object", "properties": {
            "credentials": {"type": "object", "properties": {"password": {"type": "string", "writeOnly": true}}}}});
        let model = analyze_schema(&schema).expect("supported");
        let children = group_fields(field(&model, "credentials"));
        assert_eq!(children[0].kind, FieldKind::Secret);
        assert_eq!(children[0].path, "credentials.password");
        assert_eq!(children[0].keys, vec!["credentials".to_owned(), "password".to_owned()]);
    }

    #[test]
    fn secret_state_uses_the_server_flag_then_the_value() {
        let secrets = vec![
            PluginSecretDto { path: "token".to_owned(), set: true },
            PluginSecretDto { path: "other".to_owned(), set: false },
        ];
        assert!(secret_is_set(&secrets, "token", None));
        assert!(!secret_is_set(&secrets, "other", Some(&json!("x"))));
        assert!(secret_is_set(&secrets, "unlisted", Some(&json!("${env:TOKEN}"))));
        assert!(!secret_is_set(&secrets, "unlisted", Some(&json!(""))));
        assert!(!secret_is_set(&secrets, "unlisted", None));
    }

    #[test]
    fn a_secret_keeps_its_original_value_until_the_user_types() {
        let sentinel = json!(shared::model::REDACTED_SECRET);
        assert_eq!(secret_typed_text(Some(&sentinel), Some(&sentinel)), "");
        assert_eq!(secret_typed_text(Some(&json!("abc")), Some(&sentinel)), "abc");
        assert_eq!(secret_typed_text(None, None), "");
        assert_eq!(secret_after_input("abc", Some(&sentinel)), Some(json!("abc")));
        assert_eq!(secret_after_input("", Some(&sentinel)), Some(sentinel.clone()));
        assert_eq!(secret_after_input("", None), None);
        let reference = json!("${env:TOKEN}");
        assert_eq!(secret_after_input("", Some(&reference)), Some(reference.clone()));
    }

    #[test]
    fn defaults_prefill_absent_properties_only() {
        let schema = json!({"type": "object", "properties": {
            "host": {"type": "string", "default": "localhost"},
            "port": {"type": "integer", "default": 8080},
            "verbose": {"type": "boolean", "default": true},
            "token": {"type": "string", "writeOnly": true, "default": "oops"},
            "auth": {"type": "object", "properties": {"user": {"type": "string", "default": "admin"}}}}});
        let filled = prefill_defaults(&schema, &json!({"port": 9000}));
        assert_eq!(filled["host"], "localhost");
        assert_eq!(filled["port"], 9000);
        assert_eq!(filled["verbose"], true);
        assert!(filled.get("token").is_none());
        assert_eq!(filled["auth"]["user"], "admin");

        let explicit = prefill_defaults(&schema, &json!({"host": "", "verbose": false, "auth": {"user": "bob"}}));
        assert_eq!(explicit["host"], "");
        assert_eq!(explicit["verbose"], false);
        assert_eq!(explicit["auth"]["user"], "bob");

        let from_null = prefill_defaults(&schema, &Value::Null);
        assert_eq!(from_null["port"], 8080);
    }

    #[test]
    fn a_group_without_defaults_is_not_created() {
        let schema = json!({"type": "object", "properties": {
            "auth": {"type": "object", "properties": {"user": {"type": "string"}}}}});
        assert!(prefill_defaults(&schema, &json!({})).get("auth").is_none());
    }

    #[test]
    fn unsupported_schema_leaves_the_value_untouched() {
        let schema = json!({"oneOf": []});
        let value = json!({"a": 1});
        assert_eq!(prefill_defaults(&schema, &value), value);
    }

    #[test]
    fn paths_follow_the_api_grammar() {
        assert_eq!(child_path("", "token"), "token");
        assert_eq!(child_path("credentials", "password"), "credentials.password");
        assert_eq!(item_path("tags"), "tags[]");
        let schema = json!({"type": "object", "properties": {
            "tags": {"type": "array", "items": {"type": "string"}},
            "auth": {"type": "object", "properties": {"user": {"type": "string"}}}}});
        let model = analyze_schema(&schema).expect("supported");
        assert_eq!(field(&model, "tags").path, "tags");
        assert_eq!(group_fields(field(&model, "auth"))[0].path, "auth.user");
    }

    #[test]
    fn values_are_read_and_written_by_key_path() {
        let keys = vec!["auth".to_owned(), "user".to_owned()];
        let root = json!({"auth": {"user": "a", "pass": "p"}, "x": 1});
        assert_eq!(value_at(&root, &keys), Some(&json!("a")));
        assert_eq!(value_at(&root, &["nope".to_owned(), "user".to_owned()]), None);

        let updated = set_value_at(&root, &keys, Some(json!("b")));
        assert_eq!(updated, json!({"auth": {"user": "b", "pass": "p"}, "x": 1}));
        assert_eq!(root["auth"]["user"], "a");

        assert_eq!(set_value_at(&root, &keys, None), json!({"auth": {"pass": "p"}, "x": 1}));
        assert_eq!(set_value_at(&Value::Null, &keys, Some(json!(1))), json!({"auth": {"user": 1}}));
    }

    #[test]
    fn errors_match_fields_and_the_rest_is_loose() {
        let schema = json!({"type": "object", "properties": {
            "port": {"type": "integer"},
            "tags": {"type": "array", "items": {"type": "string"}},
            "auth": {"type": "object", "properties": {"user": {"type": "string"}}}}});
        let model = analyze_schema(&schema).expect("supported");
        let error =
            |path: &str, message: &str| PluginFieldErrorDto { path: path.to_owned(), message: message.to_owned() };
        let errors = vec![
            error("port", "too small"),
            error("tags[]", "not a string"),
            error("auth.user", "required"),
            error(ROOT_PATH, "bad"),
            error("missing", "gone"),
        ];
        assert_eq!(errors_for(&errors, "port"), vec!["too small".to_owned()]);
        assert_eq!(errors_for(&errors, "tags"), vec!["not a string".to_owned()]);
        assert_eq!(errors_for(&errors, "auth.user"), vec!["required".to_owned()]);
        let loose = unmatched_errors(&model, &errors);
        assert_eq!(loose.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(), vec![ROOT_PATH, "missing"]);
    }

    #[test]
    fn tag_text_parses_by_element_kind() {
        assert_eq!(ScalarKind::Text.parse(" a "), Some(json!("a")));
        assert_eq!(ScalarKind::Integer.parse("12"), Some(json!(12)));
        assert_eq!(ScalarKind::Integer.parse("1.5"), None);
        assert_eq!(ScalarKind::Number.parse("1.5"), Some(json!(1.5)));
        assert_eq!(ScalarKind::Number.parse("x"), None);
        assert_eq!(ScalarKind::Boolean.parse("true"), Some(json!(true)));
        assert_eq!(ScalarKind::Boolean.parse("yes"), None);
        assert_eq!(scalar_text(&json!("a")), "a");
        assert_eq!(scalar_text(&json!(3)), "3");
    }

    #[test]
    fn enum_values_must_be_scalars() {
        let schema = json!({"type": "object", "properties": {"m": {"enum": [{"a": 1}]}}});
        assert_eq!(unsupported(&schema), "enum of non-scalar values");
        let empty = json!({"type": "object", "properties": {"m": {"enum": []}}});
        assert_eq!(unsupported(&empty), "enum of non-scalar values");
    }
}
