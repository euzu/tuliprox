//! `SchemaForm`: renders the settings JSON Schema a plugin ships (a draft 2020-12 **subset**)
//! with the existing form primitives.
//!
//! The component is controlled: it renders `value` and emits the whole edited
//! object through `on_change`. The schema analysis (what is supported, which
//! property is a secret, which default prefills, how a property is addressed)
//! is pure and lives in the first half of this file, apart from the rendering.

use crate::{
    app::components::{
        input::Input, number_input::NumberInput, Accordion, AccordionPanel, AppIcon, DropDownOption, DropDownSelection,
        HideContent, Select, Tag, TagList, ToggleSwitch,
    },
    i18n::use_translation,
    model::{DialogAction, DialogActions, DialogResult},
    services::DialogService,
};
use serde_json::{Map, Number, Value};
use shared::model::{PluginFieldErrorDto, PluginSecretDto};
use std::rc::Rc;
use web_sys::MouseEvent;
use yew::{component, html, use_context, use_state, Callback, Html, Properties};

// ---------------------------------------------------------------------------
// Schema analysis (pure)
// ---------------------------------------------------------------------------

/// Schema keywords the form cannot render. One of them anywhere in the schema
/// turns the whole form into a single error.
const UNSUPPORTED_KEYWORDS: [&str; 10] =
    ["oneOf", "anyOf", "allOf", "$ref", "if", "then", "else", "not", "dependentSchemas", "patternProperties"];

/// The path used for errors about the settings object as a whole.
pub const ROOT_PATH: &str = "(root)";

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

#[derive(Debug, Clone, PartialEq)]
pub enum FieldKind {
    Text,
    /// `writeOnly: true` or `format: "password"`.
    Secret,
    Integer {
        min: Option<i64>,
        max: Option<i64>,
    },
    Number,
    Boolean,
    Enum(Vec<Value>),
    List(ScalarKind),
    /// An object property one level below the root.
    Group(Vec<FieldModel>),
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
    pub kind: FieldKind,
}

impl FieldModel {
    pub fn label(&self) -> &str { self.title.as_deref().filter(|title| !title.trim().is_empty()).unwrap_or(&self.key) }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct FormModel {
    pub fields: Vec<FieldModel>,
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

fn scalar_kind_of(type_name: &str) -> Option<ScalarKind> {
    match type_name {
        "string" => Some(ScalarKind::Text),
        "integer" => Some(ScalarKind::Integer),
        "number" => Some(ScalarKind::Number),
        "boolean" => Some(ScalarKind::Boolean),
        _ => None,
    }
}

fn build_list_kind(node: &Map<String, Value>, path: &str) -> Result<FieldKind, UnsupportedSchema> {
    let Some(items) = node.get("items").and_then(Value::as_object) else {
        return Err(UnsupportedSchema::new("array without an items schema", path));
    };
    match items.get("type").and_then(Value::as_str) {
        Some("object") => Err(UnsupportedSchema::new("array of objects", path)),
        Some("array") => Err(UnsupportedSchema::new("array of arrays", path)),
        Some(name) => scalar_kind_of(name)
            .map(FieldKind::List)
            .ok_or_else(|| UnsupportedSchema::new(&format!("array of type {name}"), path)),
        None if items.contains_key("properties") => Err(UnsupportedSchema::new("array of objects", path)),
        None => Err(UnsupportedSchema::new("array items without a type", path)),
    }
}

fn build_enum_kind(values: &Value, path: &str) -> Result<FieldKind, UnsupportedSchema> {
    let values = values
        .as_array()
        .filter(|values| {
            !values.is_empty()
                && values.iter().all(|v| matches!(v, Value::String(_) | Value::Number(_) | Value::Bool(_)))
        })
        .ok_or_else(|| UnsupportedSchema::new("enum of non-scalar values", path))?;
    Ok(FieldKind::Enum(values.clone()))
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

    let kind = if let Some(values) = node.get("enum") {
        build_enum_kind(values, &path)?
    } else {
        match node.get("type") {
            Some(Value::String(type_name)) => match type_name.as_str() {
                "string" if is_secret_schema(node) => FieldKind::Secret,
                "string" => FieldKind::Text,
                "integer" => {
                    FieldKind::Integer { min: integer_bound(node, "minimum"), max: integer_bound(node, "maximum") }
                }
                "number" => FieldKind::Number,
                "boolean" => FieldKind::Boolean,
                "array" => build_list_kind(node, &path)?,
                "object" => {
                    if depth >= 1 {
                        return Err(UnsupportedSchema::new("object nested deeper than one level", &path));
                    }
                    FieldKind::Group(build_properties(node, &keys, depth + 1)?)
                }
                other => return Err(UnsupportedSchema::new(&format!("type {other}"), &path)),
            },
            Some(_) => return Err(UnsupportedSchema::new("type given as a list", &path)),
            None => return Err(UnsupportedSchema::new("property without a type", &path)),
        }
    };

    if matches!(&kind, FieldKind::Group(fields) if fields.is_empty()) {
        return Err(UnsupportedSchema::new("object without properties", &path));
    }

    Ok(FieldModel {
        key: key.to_owned(),
        keys,
        path,
        title: node.get("title").and_then(Value::as_str).map(str::to_owned),
        description: node.get("description").and_then(Value::as_str).map(str::to_owned),
        default: node.get("default").cloned(),
        required,
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
    Ok(FormModel { fields: build_properties(root, &[], 0)? })
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
                    object.remove(key);
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
        if let FieldKind::Group(children) = &field.kind {
            let mut group = match object.get(&field.key) {
                Some(Value::Object(existing)) => existing.clone(),
                _ => Map::new(),
            };
            prefill_fields(children, &mut group);
            if !group.is_empty() || object.get(&field.key).is_some_and(Value::is_object) {
                object.insert(field.key.clone(), Value::Object(group));
            }
        } else if field.kind != FieldKind::Secret && is_absent(object.get(&field.key)) {
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
        if let FieldKind::Group(children) = &field.kind {
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

#[derive(Properties, Clone, PartialEq)]
struct SchemaFieldLabelProps {
    label: String,
    field_id: String,
    description: Option<String>,
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
        Callback::from(move |event: MouseEvent| {
            event.prevent_default();
            event.stop_propagation();
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
                <button
                    class="tp__icon-button tp__field-label__help"
                    type="button"
                    title={translate.t("LABEL.HELP")}
                    onclick={on_help}
                >
                    <AppIcon name="QuestionMark"/>
                </button>
            }
        </div>
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
            <SchemaFieldLabel label={props.field.label().to_owned()} field_id={field_id.clone()}
                description={props.field.description.clone()} />
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
                error={(!props.errors.is_empty()).then(|| props.errors.join(" "))}
            />
            <small class="tp__schema-form__hint">{ translate.t("EXPLANATION.PLUGIN_SECRET_ENV_HINT") }</small>
        </div>
    }
}

fn render_enum(ctx: &FormContext<'_>, field: &FieldModel, values: &[Value], errors: &[String]) -> Html {
    let field_id = field_dom_id(&field.path);
    let current = ctx.effective(field).map(scalar_text);
    let options = Rc::new(
        values
            .iter()
            .map(|option| {
                let text = scalar_text(option);
                let selected = current.as_deref() == Some(text.as_str());
                DropDownOption::new(&text, html! { { text.clone() } }, selected)
            })
            .collect::<Vec<_>>(),
    );
    let setter = ctx.setter(field);
    let choices = values.to_vec();
    let on_select = Callback::from(move |(_, selection): (String, DropDownSelection)| {
        if let DropDownSelection::Single(id) = selection {
            if let Some(choice) = choices.iter().find(|choice| scalar_text(choice) == id) {
                setter.emit(Some(choice.clone()));
            }
        }
    });
    html! {
        <div class="tp__schema-form__field">
            <SchemaFieldLabel label={field.label().to_owned()} field_id={field_id.clone()}
                description={field.description.clone()} />
            <Select name={field_id} options={options} on_select={on_select} required={field.required}
                error={(!errors.is_empty()).then(|| errors.join(" "))} />
        </div>
    }
}

fn render_list(ctx: &FormContext<'_>, field: &FieldModel, element: ScalarKind, errors: &[String]) -> Html {
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
    let create_tag = Callback::from(move |raw: String| {
        element.parse(&raw).map(|_| Tag { label: raw.trim().to_owned(), class: None })
    });
    html! {
        <div class="tp__schema-form__field">
            <SchemaFieldLabel label={field.label().to_owned()} field_id={field_id}
                description={field.description.clone()} />
            <TagList tags={tags} readonly={false} on_change={on_change} create_tag={create_tag} />
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
            <SchemaFieldLabel label={field.label().to_owned()} field_id={field_id.clone()}
                description={field.description.clone()} />
            <NumberInput name={field.path.clone()} field_id={Some(field_id)} value={current}
                min_i64={bounds.0} max_i64={bounds.1}
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
            <SchemaFieldLabel label={field.label().to_owned()} field_id={field_id.clone()}
                description={field.description.clone()} />
            <NumberInput name={field.path.clone()} field_id={Some(field_id)} float_value={current}
                on_change_float={Some(Callback::from(move |number: Option<f64>| {
                    setter.emit(number.and_then(Number::from_f64).map(Value::Number));
                }))} />
            { render_errors(errors) }
        </div>
    }
}

fn render_field(ctx: &FormContext<'_>, field: &FieldModel) -> Html {
    let errors = errors_for(ctx.errors, &field.path);
    match &field.kind {
        FieldKind::Text => {
            let field_id = field_dom_id(&field.path);
            let setter = ctx.setter(field);
            let required = field.required;
            let current = ctx.effective(field).map(scalar_text).unwrap_or_default();
            html! {
                <div class="tp__schema-form__field">
                    <SchemaFieldLabel label={field.label().to_owned()} field_id={field_id.clone()}
                        description={field.description.clone()} />
                    <Input name={field.path.clone()} field_id={Some(field_id)} value={current} required={required}
                        on_change={Some(Callback::from(move |text: String| {
                            setter.emit(if text.is_empty() && !required { None } else { Some(Value::String(text)) });
                        }))}
                        error={(!errors.is_empty()).then(|| errors.join(" "))} />
                </div>
            }
        }
        FieldKind::Secret => {
            let value = value_at(ctx.value, &field.keys).cloned();
            let is_set = secret_is_set(ctx.secrets, &field.path, value.as_ref());
            html! {
                <SecretField key={field.path.clone()} field={Rc::new(field.clone())} value={value} is_set={is_set}
                    errors={errors} on_change={ctx.setter(field)} />
            }
        }
        FieldKind::Integer { min, max } => render_integer(ctx, field, (*min, *max), &errors),
        FieldKind::Number => render_number(ctx, field, &errors),
        FieldKind::Boolean => {
            let setter = ctx.setter(field);
            let current = ctx.effective(field).and_then(Value::as_bool).unwrap_or(false);
            html! {
                <div class="tp__schema-form__field tp__schema-form__field--inline">
                    <SchemaFieldLabel label={field.label().to_owned()} field_id={field_dom_id(&field.path)}
                        description={field.description.clone()} />
                    <ToggleSwitch value={current} aria_label={Some(field.label().to_owned())}
                        on_change={Callback::from(move |on: bool| setter.emit(Some(Value::Bool(on))))} />
                    { render_errors(&errors) }
                </div>
            }
        }
        FieldKind::Enum(values) => render_enum(ctx, field, values, &errors),
        FieldKind::List(element) => render_list(ctx, field, *element, &errors),
        FieldKind::Group(children) => html! {
            <Accordion default_panel={Some(field.path.clone())}>
                <AccordionPanel id={field.path.clone()} title={field.label().to_owned()}>
                    if let Some(description) = field.description.as_ref() {
                        <p class="tp__schema-form__hint">{ description.clone() }</p>
                    }
                    { render_errors(&errors) }
                    { for children.iter().map(|child| render_field(ctx, child)) }
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
                    { for model.fields.iter().map(|field| render_field(&ctx, field)) }
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
        assert_eq!(field(&model, "port").kind, FieldKind::Integer { min: Some(1), max: Some(65535) });
        assert_eq!(field(&model, "ratio").kind, FieldKind::Number);
        assert_eq!(field(&model, "verbose").kind, FieldKind::Boolean);
        assert_eq!(field(&model, "mode").kind, FieldKind::Enum(vec![json!("fast"), json!("slow")]));
        assert_eq!(field(&model, "tags").kind, FieldKind::List(ScalarKind::Text));
        assert_eq!(field(&model, "tags").label(), "tags");
        assert!(matches!(field(&model, "auth").kind, FieldKind::Group(_)));
        assert!(schema_supported(&schema));
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
    fn nesting_deeper_than_one_object_level_is_unsupported() {
        let schema = json!({"type": "object", "properties": {
            "a": {"type": "object", "properties": {"b": {"type": "object", "properties": {"c": {"type": "string"}}}}}}});
        let found = analyze_schema(&schema).expect_err("deep");
        assert_eq!(found.construct, "object nested deeper than one level");
        assert_eq!(found.path, "a.b");
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
        let list_type = json!({"type": "object", "properties": {"a": {"type": ["string", "null"]}}});
        assert_eq!(unsupported(&list_type), "type given as a list");
        let untyped = json!({"type": "object", "properties": {"a": {}}});
        assert_eq!(unsupported(&untyped), "property without a type");
        let null_type = json!({"type": "object", "properties": {"a": {"type": "null"}}});
        assert_eq!(unsupported(&null_type), "type null");
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
        assert_eq!(field(&model, "plain").kind, FieldKind::Text);
        assert_eq!(field(&model, "name").kind, FieldKind::Text);
    }

    #[test]
    fn secret_inside_a_group_is_detected_with_its_path() {
        let schema = json!({"type": "object", "properties": {
            "credentials": {"type": "object", "properties": {"password": {"type": "string", "writeOnly": true}}}}});
        let model = analyze_schema(&schema).expect("supported");
        let FieldKind::Group(children) = &field(&model, "credentials").kind else { panic!("group") };
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
        let FieldKind::Group(children) = &field(&model, "auth").kind else { panic!("group") };
        assert_eq!(children[0].path, "auth.user");
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
