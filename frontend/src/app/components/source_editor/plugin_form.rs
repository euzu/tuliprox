//! The forms a plugin ships for its `type: plugin` inputs and outputs.
//!
//! A plugin declares `ui.input_form` / `ui.output_form` sections in its
//! manifest. Each section is a JSON Schema over the input's or output's
//! `plugin_config` object, rendered with the same `SchemaForm` the plugin's
//! settings use. The host always shows the fields it owns (plugin, name,
//! enabled, …); the sections only add what the plugin needs.
//!
//! * `main` and `options` are the host's panels: a section with that id is
//!   shown in the same panel, below the host fields.
//! * Any other id adds a panel of its own, after them, with the section's
//!   `icon` in the sidebar.
//! * A section's `description` is shown above its fields, its `actions` as
//!   buttons below them. An action calls one of the plugin's rpc exports with
//!   the form's current, unsaved `plugin_config` and shows the answer.
//!
//! Applying the form fills in the schema defaults the user left alone and
//! checks the whole `plugin_config` against every section's schema; a
//! violation keeps the form open and is shown under its field.

use crate::{
    app::components::{prefill_defaults, translate_schema, Card, PluginUiContext, SchemaForm, TextButton},
    i18n::{use_translation, YewI18n},
};
use serde_json::{Map, Value};
use shared::{
    model::{PluginFieldErrorDto, PluginFormActionRequestDto, PluginFormSectionDto, PluginRpcKindDto},
    utils::validate_plugin_schema,
};
use std::collections::BTreeMap;
use yew::{component, html, use_state, Callback, Html, Properties};

/// Which form of a plugin a section belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginFormKind {
    Input,
    Output,
}

impl PluginFormKind {
    /// The `form` of a [`PluginFormActionRequestDto`].
    pub const fn name(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Output => "output",
        }
    }
}

/// A section with this id is shown inside the host's Main panel.
pub const SECTION_MAIN: &str = "main";
/// A section with this id is shown inside the host's Options panel.
pub const SECTION_OPTIONS: &str = "options";

/// The sidebar icon of a plugin panel that names none.
const DEFAULT_PANEL_ICON: &str = "Options";

/// The longest answer of an action that is shown, in chars.
const MAX_ACTION_RESULT_CHARS: usize = 4_000;

/// A button of a section, with its title resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginSectionAction {
    pub id: String,
    pub title: String,
    pub rpc: String,
    /// The rpc export is a `write` method: it needs write access.
    pub needs_write: bool,
}

/// One rendered section: the panel id, its localized title and its schema with
/// the plugin's own title keys resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginSection {
    pub id: String,
    pub title: String,
    /// `true` for `main` / `options`: the section renders inside the host's own
    /// card, under the host fields, so it carries no title of its own.
    pub in_host_panel: bool,
    pub schema: Value,
    pub icon: Option<String>,
    pub description: Option<String>,
    pub actions: Vec<PluginSectionAction>,
}

impl PluginSection {
    fn new(id: String, title: String, schema: Value) -> Self {
        let in_host_panel = id == SECTION_MAIN || id == SECTION_OPTIONS;
        Self { id, title, in_host_panel, schema, icon: None, description: None, actions: Vec::new() }
    }

    /// The icon of the panel's sidebar entry.
    pub fn panel_icon(&self) -> &str { self.icon.as_deref().unwrap_or(DEFAULT_PANEL_ICON) }

    /// The top-level `plugin_config` keys the section's schema declares.
    fn owns(&self, key: &str) -> bool {
        self.schema.get("properties").and_then(Value::as_object).is_some_and(|properties| properties.contains_key(key))
    }
}

/// The section's title: the plugin's `title_key` in its locale strings, then
/// its literal `title`, then the host's name for a known panel, then the id.
fn section_title(
    section: &PluginFormSectionDto,
    strings: &BTreeMap<String, String>,
    host_title: &dyn Fn(&str) -> Option<String>,
) -> String {
    section
        .title_key
        .as_ref()
        .and_then(|key| strings.get(key))
        .cloned()
        .or_else(|| section.title.clone().filter(|title| !title.trim().is_empty()))
        .or_else(|| host_title(&section.id))
        .unwrap_or_else(|| section.id.clone())
}

/// A text with an optional key: the key's translation, else the literal.
fn localized(strings: &BTreeMap<String, String>, key: Option<&String>, literal: Option<&String>) -> Option<String> {
    key.and_then(|key| strings.get(key)).or(literal).filter(|text| !text.trim().is_empty()).cloned()
}

/// The sections a plugin declares for `kind`, in manifest order. Empty when the
/// plugin is unknown, declares no form, or the UI list was not loaded.
#[must_use]
pub fn plugin_sections(
    ui: Option<&PluginUiContext>,
    plugin: &str,
    kind: PluginFormKind,
    host_title: &dyn Fn(&str) -> Option<String>,
) -> Vec<PluginSection> {
    let Some(ui) = ui else { return Vec::new() };
    let Some(entry) = ui.plugin(plugin) else { return Vec::new() };
    let strings = ui.strings_for(plugin);
    let sections = match kind {
        PluginFormKind::Input => &entry.input_form,
        PluginFormKind::Output => &entry.output_form,
    };
    let needs_write =
        |rpc: &str| entry.rpc_exports.iter().any(|export| export.name == rpc && export.kind == PluginRpcKindDto::Write);
    sections
        .iter()
        .map(|section| {
            let mut rendered = PluginSection::new(
                section.id.clone(),
                section_title(section, &strings, host_title),
                translate_schema(&section.schema, &strings),
            );
            rendered.icon = section.icon.clone().filter(|icon| !icon.trim().is_empty());
            rendered.description = localized(&strings, section.description_key.as_ref(), section.description.as_ref());
            rendered.actions = section
                .actions
                .iter()
                .map(|action| PluginSectionAction {
                    id: action.id.clone(),
                    title: localized(&strings, action.title_key.as_ref(), Some(&action.title))
                        .unwrap_or_else(|| action.id.clone()),
                    rpc: action.rpc.clone(),
                    needs_write: needs_write(&action.rpc),
                })
                .collect();
            rendered
        })
        .collect()
}

/// The host's translated name for a panel id, for a section that names no title
/// of its own.
#[must_use]
pub fn host_section_title(id: &str, translate: &YewI18n) -> Option<String> {
    match id {
        SECTION_MAIN => Some(translate.t("LABEL.MAIN")),
        SECTION_OPTIONS => Some(translate.t("LABEL.OPTIONS")),
        _ => None,
    }
}

/// The sections to render in `panel`: the `main` one on the Main panel, the
/// `options` one on the Options panel, every other id on its own.
#[must_use]
pub fn sections_for_panel<'a>(sections: &'a [PluginSection], panel: &str) -> Vec<&'a PluginSection> {
    sections.iter().filter(|section| section.id == panel).collect()
}

/// Whether picking `next` replaces the plugin in use. The select emits on every
/// click, the already selected option included, so only a *different* id may
/// drop the values it owns.
#[must_use]
pub fn replaces_plugin(current: &str, next: Option<&str>) -> bool { next.is_some_and(|next| next != current) }

/// `config` with every value a section's schema has a `default` for, and the
/// user left alone, filled in. This is what applying the form stores.
#[must_use]
pub fn prefill_sections(sections: &[PluginSection], config: &Map<String, Value>) -> Map<String, Value> {
    let filled =
        sections.iter().fold(Value::Object(config.clone()), |value, section| prefill_defaults(&section.schema, &value));
    match filled {
        Value::Object(map) => map,
        _ => config.clone(),
    }
}

/// One object schema over the whole `plugin_config`: the union of the
/// sections' `properties` and `required`. `additionalProperties` of a section
/// is left out, since every section only describes its own keys.
#[must_use]
pub fn merged_schema(sections: &[PluginSection]) -> Value {
    let mut properties = Map::new();
    let mut required: Vec<Value> = Vec::new();
    for section in sections {
        if let Some(Value::Object(own)) = section.schema.get("properties") {
            properties.extend(own.iter().map(|(key, value)| (key.clone(), value.clone())));
        }
        if let Some(Value::Array(names)) = section.schema.get("required") {
            for name in names {
                if !required.contains(name) {
                    required.push(name.clone());
                }
            }
        }
    }
    let mut merged = Map::new();
    merged.insert("type".to_owned(), Value::String("object".to_owned()));
    merged.insert("properties".to_owned(), Value::Object(properties));
    merged.insert("required".to_owned(), Value::Array(required));
    Value::Object(merged)
}

/// Every violation of `config` against the sections' schemas.
#[must_use]
pub fn validate_sections(sections: &[PluginSection], config: &Map<String, Value>) -> Vec<PluginFieldErrorDto> {
    if sections.is_empty() {
        return Vec::new();
    }
    validate_plugin_schema(&merged_schema(sections), &Value::Object(config.clone()))
}

/// The top-level `plugin_config` key an error path is about: `a` for `a`,
/// `a.b` and `a[]`.
fn top_key(path: &str) -> &str { path.split(['.', '[']).next().unwrap_or(path) }

/// The errors that belong to `section`: those about one of its keys.
#[must_use]
pub fn section_errors(section: &PluginSection, errors: &[PluginFieldErrorDto]) -> Vec<PluginFieldErrorDto> {
    errors.iter().filter(|error| section.owns(top_key(&error.path))).cloned().collect()
}

/// The panel of the first section with an error, to show after a refused apply.
#[must_use]
pub fn first_panel_with_errors(sections: &[PluginSection], errors: &[PluginFieldErrorDto]) -> Option<String> {
    sections.iter().find(|section| !section_errors(section, errors).is_empty()).map(|section| section.id.clone())
}

/// What an action answered, ready to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionOutcome {
    pub ok: bool,
    pub text: String,
    /// The text is JSON and keeps its layout.
    pub preformatted: bool,
}

fn shortened(text: &str) -> String {
    if text.chars().count() <= MAX_ACTION_RESULT_CHARS {
        text.to_owned()
    } else {
        let mut short: String = text.chars().take(MAX_ACTION_RESULT_CHARS).collect();
        short.push('…');
        short
    }
}

/// The answer of an action: a string is shown as it is, `{message, ok?}` as its
/// message (an error when `ok` is `false`), anything else as JSON. `None` for
/// `null`, which the caller shows as a plain "done".
#[must_use]
pub fn action_outcome(answer: &Value) -> Option<ActionOutcome> {
    match answer {
        Value::Null => None,
        Value::String(text) => Some(ActionOutcome { ok: true, text: shortened(text), preformatted: false }),
        Value::Object(map) if map.get("message").is_some_and(Value::is_string) => Some(ActionOutcome {
            ok: map.get("ok").and_then(Value::as_bool).unwrap_or(true),
            text: shortened(map.get("message").and_then(Value::as_str).unwrap_or_default()),
            preformatted: false,
        }),
        other => Some(ActionOutcome {
            ok: true,
            text: shortened(&serde_json::to_string_pretty(other).unwrap_or_default()),
            preformatted: true,
        }),
    }
}

#[derive(Properties, Clone, PartialEq)]
pub struct PluginSectionCardProps {
    pub section: PluginSection,
    /// The whole `plugin_config` object; the section edits its own keys.
    pub value: Value,
    pub allow_write: bool,
    pub on_change: Callback<Value>,
    /// The violations of the last apply, all sections together; the card shows its own.
    #[prop_or_default]
    pub errors: Vec<PluginFieldErrorDto>,
    /// The plugin the form belongs to, for the actions.
    #[prop_or_default]
    pub plugin_id: String,
    #[prop_or(PluginFormKind::Input)]
    pub form: PluginFormKind,
}

/// One section's fields. A host-panel section renders bare, so it sits inside the
/// host's card; a panel of the plugin's own gets a card with its title.
#[component]
pub fn PluginSectionCard(props: &PluginSectionCardProps) -> Html {
    let on_change = props.on_change.clone();
    let errors = section_errors(&props.section, &props.errors);
    let fields = html! {
        <>
            if let Some(description) = props.section.description.clone() {
                <p class="tp__plugin-form__section-description">{ description }</p>
            }
            <fieldset class="tp__plugin-ext__fields" disabled={!props.allow_write}>
                <SchemaForm schema={props.section.schema.clone()} value={props.value.clone()} {errors} {on_change} />
            </fieldset>
            if !props.section.actions.is_empty() {
                <PluginFormActions plugin_id={props.plugin_id.clone()} form={props.form}
                    section={props.section.id.clone()} actions={props.section.actions.clone()}
                    config={props.value.clone()} allow_write={props.allow_write} />
            }
        </>
    };
    if props.section.in_host_panel {
        fields
    } else {
        html! {
            <Card class="tp__config-view__card">
                <div class="tp__plugin-form__section">
                    <div class="tp__plugin-form__section-title">{ props.section.title.clone() }</div>
                    { fields }
                </div>
            </Card>
        }
    }
}

#[derive(Properties, Clone, PartialEq)]
struct PluginFormActionsProps {
    plugin_id: String,
    form: PluginFormKind,
    section: String,
    actions: Vec<PluginSectionAction>,
    config: Value,
    allow_write: bool,
}

/// The buttons of a section and the answer of the last one clicked.
#[component]
fn PluginFormActions(props: &PluginFormActionsProps) -> Html {
    let translate = use_translation();
    let ui = crate::app::components::use_plugin_ui();
    let running = use_state(|| None::<String>);
    let outcome = use_state(|| None::<(String, ActionOutcome)>);

    let on_click = {
        let running = running.clone();
        let outcome = outcome.clone();
        let actions = props.actions.clone();
        let request = (props.plugin_id.clone(), props.form, props.section.clone(), props.config.clone());
        let done = translate.t("MESSAGES.PLUGIN_ACTION_DONE");
        Callback::from(move |id: String| {
            let (Some(ui), Some(action)) = (ui.clone(), actions.iter().find(|action| action.id == id).cloned()) else {
                return;
            };
            if running.is_some() {
                return;
            }
            let (plugin_id, form, section, config) = request.clone();
            let payload = serde_json::to_value(PluginFormActionRequestDto {
                form: form.name().to_owned(),
                section,
                action: action.id.clone(),
                config,
            })
            .unwrap_or(Value::Null);
            running.set(Some(action.id.clone()));
            outcome.set(None);
            let running = running.clone();
            let outcome = outcome.clone();
            let done = done.clone();
            yew::platform::spawn_local(async move {
                let shown = match ui.service.rpc(&plugin_id, &action.rpc, &payload).await {
                    Ok(answer) => {
                        action_outcome(&answer).unwrap_or(ActionOutcome { ok: true, text: done, preformatted: false })
                    }
                    Err(failure) => ActionOutcome { ok: false, text: shortened(&failure.error), preformatted: false },
                };
                outcome.set(Some((action.title.clone(), shown)));
                running.set(None);
            });
        })
    };

    html! {
        <div class="tp__plugin-form__actions">
            <div class="tp__plugin-form__action-buttons">
                { for props.actions.iter().map(|action| {
                    let busy = running.as_deref() == Some(action.id.as_str());
                    let title = if busy { translate.t("MESSAGES.PLUGIN_ACTION_RUNNING") } else { action.title.clone() };
                    html! {
                        <TextButton class="secondary" name={action.id.clone()} icon="PlayArrow" title={title}
                            disabled={running.is_some() || (action.needs_write && !props.allow_write)}
                            onclick={on_click.clone()} />
                    }
                }) }
            </div>
            if let Some((title, shown)) = (*outcome).clone() {
                <div class={if shown.ok { "tp__plugin-form__action-result" } else { "tp__plugin-form__action-result tp__plugin-form__action-result--error" }}
                    role={if shown.ok { "status" } else { "alert" }}>
                    <span class="tp__plugin-form__action-result-title">{ title }</span>
                    if shown.preformatted {
                        <pre>{ shown.text }</pre>
                    } else {
                        <span>{ shown.text }</span>
                    }
                </div>
            }
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn section(id: &str, title: Option<&str>, title_key: Option<&str>) -> PluginFormSectionDto {
        PluginFormSectionDto {
            id: id.to_string(),
            title: title.map(ToString::to_string),
            title_key: title_key.map(ToString::to_string),
            icon: None,
            description: None,
            description_key: None,
            schema: json!({"type": "object", "properties": {"target": {"type": "string"}}}),
            actions: Vec::new(),
        }
    }

    fn host_title(id: &str) -> Option<String> { (id == SECTION_MAIN).then(|| "Main".to_string()) }

    fn rendered(id: &str, schema: Value) -> PluginSection { PluginSection::new(id.to_string(), id.to_string(), schema) }

    #[test]
    fn the_title_prefers_the_plugin_locale_then_the_literal_then_the_host_name() {
        let mut strings = BTreeMap::new();
        strings.insert("form.channels".to_string(), "Kanäle".to_string());

        let titled = section("channels", Some("Channels"), Some("form.channels"));
        assert_eq!(section_title(&titled, &strings, &host_title), "Kanäle");
        let literal = section("channels", Some("Channels"), None);
        assert_eq!(section_title(&literal, &strings, &host_title), "Channels");
        let unknown_key = section("channels", None, Some("missing"));
        assert_eq!(section_title(&unknown_key, &strings, &host_title), "channels");
        let main = section("main", None, None);
        assert_eq!(section_title(&main, &strings, &host_title), "Main");
    }

    #[test]
    fn descriptions_prefer_the_locale_and_skip_blanks() {
        let mut strings = BTreeMap::new();
        strings.insert("d".to_string(), "Beschreibung".to_string());
        let literal = "Description".to_string();
        let blank = "  ".to_string();
        assert_eq!(localized(&strings, Some(&"d".to_string()), Some(&literal)).as_deref(), Some("Beschreibung"));
        assert_eq!(localized(&strings, Some(&"x".to_string()), Some(&literal)).as_deref(), Some("Description"));
        assert_eq!(localized(&strings, None, Some(&blank)), None);
    }

    #[test]
    fn only_a_different_plugin_replaces_the_selected_one() {
        assert!(!replaces_plugin("youtube-input", Some("youtube-input")), "re-picking keeps the values");
        assert!(!replaces_plugin("youtube-input", None), "an empty answer is not a new plugin");
        assert!(replaces_plugin("youtube-input", Some("csv-writer")));
    }

    #[test]
    fn a_panel_shows_only_its_own_sections() {
        let sections = vec![
            PluginSection::new("main".to_string(), "Main".to_string(), json!({})),
            PluginSection::new("channels".to_string(), "Channels".to_string(), json!({})),
            PluginSection::new("options".to_string(), "Options".to_string(), json!({})),
        ];

        let ids = |panel: &str| sections_for_panel(&sections, panel).iter().map(|s| s.id.clone()).collect::<Vec<_>>();

        assert_eq!(ids(SECTION_MAIN), ["main"]);
        assert_eq!(ids(SECTION_OPTIONS), ["options"]);
        assert_eq!(ids("channels"), ["channels"]);
        assert_eq!(ids("unknown"), Vec::<String>::new());
        assert!(sections[0].in_host_panel && sections[2].in_host_panel, "host panels carry no title of their own");
        assert!(!sections[1].in_host_panel, "a plugin panel keeps its title card");
        assert_eq!(sections[1].panel_icon(), DEFAULT_PANEL_ICON);
    }

    #[test]
    fn applying_fills_defaults_and_checks_every_section() {
        let sections = vec![
            rendered(
                "main",
                json!({"type": "object", "required": ["url"], "properties": {
                "url": {"type": "string", "format": "uri"}, "mode": {"enum": ["a", "b"], "default": "a"}}}),
            ),
            rendered(
                "limits",
                json!({"type": "object", "additionalProperties": false, "properties": {
                "count": {"type": "integer", "minimum": 1, "default": 5}}}),
            ),
        ];
        let filled = prefill_sections(&sections, &Map::new());
        assert_eq!(Value::Object(filled.clone()), json!({"mode": "a", "count": 5}));

        let errors = validate_sections(&sections, &filled);
        assert_eq!(errors.iter().map(|error| error.path.as_str()).collect::<Vec<_>>(), ["url"]);
        assert_eq!(first_panel_with_errors(&sections, &errors).as_deref(), Some("main"));

        let mut config = filled;
        config.insert("url".to_string(), json!("https://example.com"));
        config.insert("count".to_string(), json!(0));
        let errors = validate_sections(&sections, &config);
        assert_eq!(errors.iter().map(|error| error.path.as_str()).collect::<Vec<_>>(), ["count"]);
        assert_eq!(first_panel_with_errors(&sections, &errors).as_deref(), Some("limits"));
        assert!(section_errors(&sections[0], &errors).is_empty(), "the main card shows none of them");
        assert_eq!(section_errors(&sections[1], &errors).len(), 1);
        // One section's `additionalProperties: false` does not refuse another section's keys.
        let other_keys = validate_sections(&sections, &Map::from_iter([("url".to_string(), json!("https://x"))]));
        assert_eq!(other_keys, Vec::<PluginFieldErrorDto>::new());
    }

    #[test]
    fn nested_and_list_errors_belong_to_their_top_level_key() {
        assert_eq!(top_key("auth.user"), "auth");
        assert_eq!(top_key("tags[]"), "tags");
        assert_eq!(top_key("plain"), "plain");
    }

    #[test]
    fn action_answers_are_shown_by_shape() {
        assert_eq!(action_outcome(&Value::Null), None);
        assert_eq!(
            action_outcome(&json!("12 channels found")),
            Some(ActionOutcome { ok: true, text: "12 channels found".into(), preformatted: false })
        );
        assert_eq!(
            action_outcome(&json!({"ok": false, "message": "not reachable"})),
            Some(ActionOutcome { ok: false, text: "not reachable".into(), preformatted: false })
        );
        let json_answer = action_outcome(&json!({"count": 3})).unwrap();
        assert!(json_answer.ok && json_answer.preformatted && json_answer.text.contains("\"count\": 3"));
        let long = action_outcome(&json!("x".repeat(MAX_ACTION_RESULT_CHARS + 10))).unwrap();
        assert_eq!(long.text.chars().count(), MAX_ACTION_RESULT_CHARS + 1);
    }

    #[test]
    fn the_action_request_names_its_form() {
        assert_eq!(PluginFormKind::Input.name(), "input");
        assert_eq!(PluginFormKind::Output.name(), "output");
        let request = serde_json::to_value(PluginFormActionRequestDto {
            form: "input".into(),
            section: "main".into(),
            action: "check".into(),
            config: json!({"a": 1}),
        })
        .unwrap();
        assert_eq!(request, json!({"form": "input", "section": "main", "action": "check", "config": {"a": 1}}));
    }

    #[test]
    fn the_form_and_widget_keys_exist_in_every_locale() {
        for locale in [
            include_str!("../../../../public/assets/i18n/en.json"),
            include_str!("../../../../public/assets/i18n/es.json"),
            include_str!("../../../../public/assets/i18n/ar.json"),
            include_str!("../../../../public/assets/i18n/ru.json"),
        ] {
            let translations: Value = serde_json::from_str(locale).unwrap();
            for key in [
                "MESSAGES.PLUGIN_ACTION_DONE",
                "MESSAGES.PLUGIN_ACTION_RUNNING",
                "MESSAGES.PLUGIN_FORM_INVALID",
                "LABEL.DOCUMENTATION",
                "LABEL.PLUGIN_DURATION_SECONDS",
                "LABEL.PLUGIN_DURATION_MINUTES",
                "LABEL.PLUGIN_DURATION_HOURS",
                "LABEL.PLUGIN_DURATION_DAYS",
            ] {
                let pointer = format!("/{}", key.replace('.', "/"));
                assert!(translations.pointer(&pointer).and_then(Value::as_str).is_some(), "{pointer}");
            }
        }
    }
}
