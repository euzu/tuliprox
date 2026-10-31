//! Plugin settings on the config page (plan §11 *Config page integration*). W6b lane U.
//!
//! A plugin can put three things on the config page: fields inside a built-in
//! section (`ui.config_extensions`, stored under `ext.<section>.<field>`), a
//! section of its own after the built-in ones (`ui.config_section`), and a tab
//! (`ui.config_tab`, a schema form or a sandboxed frame). All of them edit the
//! plugin's settings, so the page keeps one draft per plugin and saves it with
//! one `PUT …/settings` when the page is saved.

use super::config_view_context::ConfigViewContext;
use crate::{
    app::components::{
        plugin_available, prefill_defaults, translate_schema, use_plugin_ui, PluginBadge, PluginFrame, PluginSlot,
        PluginUnavailable, SchemaForm,
    },
    hooks::{use_service_context, Services},
    i18n::{use_translation, YewI18n},
    services::PluginSaveError,
};
use serde_json::{Map, Value};
use shared::model::{
    PluginFieldErrorDto, PluginSecretDto, PluginSettingsDto, PluginSettingsSaveDto, PluginSlotDto, PluginUiDto,
};
use std::{collections::BTreeMap, rc::Rc};
use yew::{
    component, hook, html, platform::spawn_local, use_context, use_effect_with, use_reducer, Callback, Html,
    Properties, Reducible, UseReducerHandle,
};

/// Key of the injected fields inside a plugin's settings: `ext.<section>.<field>`.
const EXT_KEY: &str = "ext";

// ---------------------------------------------------------------------------
// Settings paths (pure)
// ---------------------------------------------------------------------------

/// The `ext.<section>` object of the settings; empty when there is none.
pub fn section_value(settings: &Value, section: &str) -> Value {
    settings
        .get(EXT_KEY)
        .and_then(|ext| ext.get(section))
        .filter(|value| value.is_object())
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()))
}

/// The settings with `ext.<section>` replaced.
pub fn with_section(settings: &Value, section: &str, value: Value) -> Value {
    let mut result = settings.as_object().cloned().unwrap_or_default();
    let mut ext = result.get(EXT_KEY).and_then(Value::as_object).cloned().unwrap_or_default();
    ext.insert(section.to_owned(), value);
    result.insert(EXT_KEY.to_owned(), Value::Object(ext));
    Value::Object(result)
}

/// The settings without the injected `ext` fields: what the plugin's own schema describes.
pub fn main_value(settings: &Value) -> Value {
    let mut result = settings.as_object().cloned().unwrap_or_default();
    result.remove(EXT_KEY);
    Value::Object(result)
}

/// The settings with their own part replaced by `main`, keeping the injected fields.
pub fn with_main(settings: &Value, main: &Value) -> Value {
    let mut result = main.as_object().cloned().unwrap_or_default();
    result.remove(EXT_KEY);
    if let Some(ext) = settings.get(EXT_KEY) {
        result.insert(EXT_KEY.to_owned(), ext.clone());
    }
    Value::Object(result)
}

fn ext_prefix(section: &str) -> String { format!("{EXT_KEY}.{section}") }

/// The secrets of an injected section, with paths relative to the fragment.
pub fn section_secrets(secrets: &[PluginSecretDto], section: &str) -> Vec<PluginSecretDto> {
    let prefix = format!("{}.", ext_prefix(section));
    secrets
        .iter()
        .filter_map(|secret| {
            secret.path.strip_prefix(&prefix).map(|path| PluginSecretDto { path: path.to_owned(), set: secret.set })
        })
        .collect()
}

/// The secrets of the plugin's own schema.
pub fn main_secrets(secrets: &[PluginSecretDto]) -> Vec<PluginSecretDto> {
    secrets.iter().filter(|secret| !secret.path.starts_with(&format!("{EXT_KEY}."))).cloned().collect()
}

/// The errors of an injected section, with paths relative to the fragment.
pub fn section_errors(errors: &[PluginFieldErrorDto], section: &str) -> Vec<PluginFieldErrorDto> {
    let prefix = ext_prefix(section);
    errors
        .iter()
        .filter_map(|error| {
            let path = if error.path == prefix {
                Some("(root)")
            } else {
                error.path.strip_prefix(&format!("{prefix}.")).filter(|rest| !rest.is_empty())
            };
            path.map(|path| PluginFieldErrorDto { path: path.to_owned(), message: error.message.clone() })
        })
        .collect()
}

/// The errors of the plugin's own settings: everything that is not under `ext`.
pub fn main_errors(errors: &[PluginFieldErrorDto]) -> Vec<PluginFieldErrorDto> {
    errors
        .iter()
        .filter(|error| error.path != EXT_KEY && !error.path.starts_with(&format!("{EXT_KEY}.")))
        .cloned()
        .collect()
}

/// The settings as the form starts from them: schema defaults filled in, for
/// the plugin's own schema and for every injected fragment.
pub fn baseline(settings: &Value, schema: Option<&Value>, extensions: &[(String, Value)]) -> Value {
    let mut result = settings.as_object().cloned().map_or_else(|| Value::Object(Map::new()), Value::Object);
    if let Some(schema) = schema {
        let prefilled = prefill_defaults(schema, &main_value(&result));
        result = with_main(&result, &prefilled);
    }
    for (section, fragment) in extensions {
        let existing = section_value(&result, section);
        let prefilled = prefill_defaults(fragment, &existing);
        if prefilled != existing || result.get(EXT_KEY).and_then(|ext| ext.get(section)).is_some() {
            result = with_section(&result, section, prefilled);
        }
    }
    result
}

// ---------------------------------------------------------------------------
// The drafts (pure reducer)
// ---------------------------------------------------------------------------

/// One plugin's settings while the config page is open.
#[derive(Clone, PartialEq, Debug)]
pub struct PluginDraft {
    /// As the server holds them, defaults filled in.
    pub saved: Value,
    /// What the forms edit.
    pub draft: Value,
    pub schema: Option<Value>,
    pub secrets: Vec<PluginSecretDto>,
    /// `(section, fragment schema)` of the plugin's `config_extensions`.
    pub extensions: Vec<(String, Value)>,
    /// A `422` of the last save: the message, and the violations by property.
    pub error: Option<String>,
    pub errors: Vec<PluginFieldErrorDto>,
    /// Bumped whenever the forms must be rebuilt from `draft`.
    pub generation: u32,
}

impl PluginDraft {
    pub fn is_modified(&self) -> bool { self.draft != self.saved }
}

#[derive(Clone, PartialEq, Default, Debug)]
pub struct PluginSettingsState {
    pub plugins: BTreeMap<String, PluginDraft>,
    /// Plugins whose settings could not be loaded, with the message.
    pub load_errors: BTreeMap<String, String>,
}

pub enum PluginSettingsAction {
    Loaded {
        id: String,
        settings: PluginSettingsDto,
        extensions: Vec<(String, Value)>,
    },
    LoadFailed {
        id: String,
        message: String,
    },
    EditSection {
        id: String,
        section: String,
        value: Value,
    },
    EditMain {
        id: String,
        value: Value,
    },
    /// A save of the page went through.
    Saved {
        id: String,
        settings: PluginSettingsDto,
    },
    /// The server refused a save with a `422`.
    Rejected {
        id: String,
        error: String,
        fields: Vec<PluginFieldErrorDto>,
    },
    /// Throw away every unsaved edit.
    Reset,
}

impl PluginSettingsState {
    pub fn has_changes(&self) -> bool { self.plugins.values().any(PluginDraft::is_modified) }

    /// The built-in sections that have injected fields with unsaved edits.
    pub fn modified_sections(&self) -> Vec<String> {
        let mut sections = Vec::new();
        for draft in self.plugins.values() {
            for (section, _) in &draft.extensions {
                if section_value(&draft.draft, section) != section_value(&draft.saved, section)
                    && !sections.contains(section)
                {
                    sections.push(section.clone());
                }
            }
        }
        sections
    }

    fn load(
        &mut self,
        id: String,
        settings: &PluginSettingsDto,
        extensions: Vec<(String, Value)>,
        previous: Option<&PluginDraft>,
    ) {
        let schema = settings.schema.clone().or_else(|| previous.and_then(|draft| draft.schema.clone()));
        let base = baseline(&settings.settings, schema.as_ref(), &extensions);
        let generation = previous.map_or(0, |draft| draft.generation + 1);
        self.load_errors.remove(&id);
        self.plugins.insert(
            id,
            PluginDraft {
                saved: base.clone(),
                draft: base,
                schema,
                secrets: settings.secrets.clone(),
                extensions,
                error: None,
                errors: Vec::new(),
                generation,
            },
        );
    }
}

impl Reducible for PluginSettingsState {
    type Action = PluginSettingsAction;

    fn reduce(self: Rc<Self>, action: PluginSettingsAction) -> Rc<Self> {
        let mut next = (*self).clone();
        match action {
            PluginSettingsAction::Loaded { id, settings, extensions } => {
                let previous = self.plugins.get(&id);
                next.load(id, &settings, extensions, previous);
            }
            PluginSettingsAction::LoadFailed { id, message } => {
                next.load_errors.insert(id, message);
            }
            PluginSettingsAction::EditSection { id, section, value } => {
                if let Some(draft) = next.plugins.get_mut(&id) {
                    draft.draft = with_section(&draft.draft, &section, value);
                }
            }
            PluginSettingsAction::EditMain { id, value } => {
                if let Some(draft) = next.plugins.get_mut(&id) {
                    draft.draft = with_main(&draft.draft, &value);
                }
            }
            PluginSettingsAction::Saved { id, settings } => {
                let previous = self.plugins.get(&id);
                let extensions = previous.map(|draft| draft.extensions.clone()).unwrap_or_default();
                next.load(id, &settings, extensions, previous);
            }
            PluginSettingsAction::Rejected { id, error, fields } => {
                if let Some(draft) = next.plugins.get_mut(&id) {
                    draft.error = Some(error);
                    draft.errors = fields;
                }
            }
            PluginSettingsAction::Reset => {
                for draft in next.plugins.values_mut() {
                    if draft.is_modified() || draft.error.is_some() {
                        draft.draft = draft.saved.clone();
                        draft.error = None;
                        draft.errors = Vec::new();
                        draft.generation += 1;
                    }
                }
            }
        }
        Rc::new(next)
    }
}

/// What the plugin parts of the config page read and write.
#[derive(Clone)]
pub struct PluginSettingsContext {
    pub state: Rc<PluginSettingsState>,
    pub dispatch: Callback<PluginSettingsAction>,
}

impl PartialEq for PluginSettingsContext {
    fn eq(&self, _other: &Self) -> bool { false }
}

/// Whether the plugin's settings have to be loaded for the config page.
fn needs_settings(plugin: &PluginUiDto) -> bool {
    plugin_available(plugin)
        && (plugin.config_section
            || !plugin.config_extensions.is_empty()
            || plugin.config_tab.as_ref().is_some_and(|tab| tab.schema.is_some()))
}

/// The drafts of the config page. Loads the settings of every plugin that
/// contributes to the page, and throws the edits away when edit mode ends.
#[hook]
pub fn use_plugin_settings(edit_mode: bool, enabled: bool) -> UseReducerHandle<PluginSettingsState> {
    let services = use_service_context();
    let ui = use_plugin_ui();
    let state = use_reducer(PluginSettingsState::default);

    let wanted: Vec<(String, Vec<(String, Value)>)> = if enabled {
        ui.as_ref()
            .map(|ui| {
                ui.plugins
                    .iter()
                    .filter(|plugin| needs_settings(plugin))
                    .map(|plugin| {
                        let extensions = plugin
                            .config_extensions
                            .iter()
                            .map(|ext| (ext.section.clone(), ext.schema.clone()))
                            .collect();
                        (plugin.id.clone(), extensions)
                    })
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    {
        let dispatcher = state.dispatcher();
        let services = services.clone();
        use_effect_with(wanted, move |wanted| {
            for (id, extensions) in wanted.clone() {
                let dispatcher = dispatcher.clone();
                let services = services.clone();
                spawn_local(async move {
                    match services.plugins.get_settings(&id).await {
                        Ok(settings) => dispatcher.dispatch(PluginSettingsAction::Loaded { id, settings, extensions }),
                        Err(err) => {
                            dispatcher.dispatch(PluginSettingsAction::LoadFailed { id, message: err.to_string() });
                        }
                    }
                });
            }
        });
    }

    {
        let dispatcher = state.dispatcher();
        use_effect_with(edit_mode, move |edit_mode| {
            if !*edit_mode {
                dispatcher.dispatch(PluginSettingsAction::Reset);
            }
        });
    }

    state
}

/// Saves every plugin with unsaved edits: one `PUT …/settings` per plugin, with
/// the draft merged (own settings plus every injected section). Returns whether
/// all of them went through.
pub async fn save_modified_plugins(
    services: Rc<Services>,
    translate: YewI18n,
    saves: Vec<(String, String, Value)>,
    dispatch: Callback<PluginSettingsAction>,
) -> bool {
    let mut all_saved = true;
    for (id, name, settings) in saves {
        match services.plugins.save_settings(&id, &PluginSettingsSaveDto { settings }).await {
            Ok(saved) => {
                services.toastr.success(format!("{name}: {}", translate.t("MESSAGES.PLUGIN_SETTINGS_SAVED")));
                for warning in &saved.warnings {
                    services.toastr.warning(format!("{name}: {warning}"));
                }
                dispatch.emit(PluginSettingsAction::Saved { id, settings: saved });
            }
            Err(PluginSaveError::Invalid(rejection)) => {
                all_saved = false;
                services.toastr.error(format!("{name}: {}", rejection.error));
                dispatch.emit(PluginSettingsAction::Rejected { id, error: rejection.error, fields: rejection.fields });
            }
            Err(PluginSaveError::Other(message)) => {
                all_saved = false;
                services.toastr.error(format!("{name}: {message}"));
            }
        }
    }
    all_saved
}

// ---------------------------------------------------------------------------
// Components
// ---------------------------------------------------------------------------

#[hook]
fn use_edit_mode() -> bool { use_context::<ConfigViewContext>().is_some_and(|context| *context.edit_mode) }

#[derive(Properties, PartialEq)]
pub struct PluginSettingsNoticeProps {
    pub id: String,
}

/// Why a plugin's settings are not shown yet: still loading, or the load failed.
#[component]
fn PluginSettingsNotice(props: &PluginSettingsNoticeProps) -> Html {
    let translate = use_translation();
    let context = use_context::<PluginSettingsContext>();
    match context.and_then(|context| context.state.load_errors.get(&props.id).cloned()) {
        Some(message) => html! {
            <div class="tp__plugin__reason tp__input-error" role="alert">
                { format!("{} {message}", translate.t("MESSAGES.PLUGIN_SETTINGS_LOAD_FAILED")) }
            </div>
        },
        None => html! { <div class="tp__plugin__reason">{ translate.t("LABEL.LOADING") }</div> },
    }
}

fn render_form_errors(draft: &PluginDraft) -> Html {
    draft.error.as_ref().map_or_else(Html::default, |error| {
        html! { <div class="tp__plugin-ext__error tp__input-error" role="alert">{ error.clone() }</div> }
    })
}

#[derive(Properties, PartialEq)]
pub struct PluginConfigExtensionsProps {
    /// The id of the built-in `ConfigPage` section.
    pub section: String,
}

/// The fields plugins inject into one built-in config section, under a divider
/// that names the plugin.
#[component]
pub fn PluginConfigExtensions(props: &PluginConfigExtensionsProps) -> Html {
    let ui = use_plugin_ui();
    let context = use_context::<PluginSettingsContext>();
    let editing = use_edit_mode();
    let (Some(ui), Some(context)) = (ui, context) else { return html! {} };
    let entries: Vec<_> = ui
        .plugins
        .iter()
        .flat_map(|plugin| {
            plugin
                .config_extensions
                .iter()
                .filter(|extension| extension.section == props.section)
                .map(move |extension| (Rc::clone(plugin), extension.clone()))
        })
        .collect();
    if entries.is_empty() {
        return html! {};
    }
    html! {
        <div class="tp__plugin-ext__group">
            { for entries.iter().map(|(plugin, extension)| {
                let key = format!("{}:{}", plugin.id, extension.section);
                if !plugin_available(plugin) {
                    return html! { <PluginUnavailable key={key} plugin={Rc::clone(plugin)} title={String::new()} /> };
                }
                let Some(draft) = context.state.plugins.get(&plugin.id) else {
                    return html! { <div key={key}><PluginBadge name={plugin.name.clone()} /><PluginSettingsNotice id={plugin.id.clone()} /></div> };
                };
                let strings = ui.strings_for(&plugin.id);
                let on_change = {
                    let dispatch = context.dispatch.clone();
                    let id = plugin.id.clone();
                    let section = extension.section.clone();
                    Callback::from(move |value: Value| {
                        dispatch.emit(PluginSettingsAction::EditSection { id: id.clone(), section: section.clone(), value });
                    })
                };
                html! {
                    <div key={key} class="tp__plugin-ext">
                        <div class="tp__plugin-ext__divider"><PluginBadge name={plugin.name.clone()} /></div>
                        { render_form_errors(draft) }
                        <fieldset class="tp__plugin-ext__fields" disabled={!editing}>
                            <SchemaForm
                                key={draft.generation.to_string()}
                                schema={translate_schema(&extension.schema, &strings)}
                                value={section_value(&draft.draft, &extension.section)}
                                secrets={section_secrets(&draft.secrets, &extension.section)}
                                errors={section_errors(&draft.errors, &extension.section)}
                                on_change={on_change}
                            />
                        </fieldset>
                    </div>
                }
            }) }
        </div>
    }
}

/// The plugin sections after the built-in ones: the schema form of every
/// plugin with `ui.config_section`, then the frames contributed to the
/// `config-section` slot.
#[component]
pub fn PluginConfigSections() -> Html {
    let ui = use_plugin_ui();
    let context = use_context::<PluginSettingsContext>();
    let translate = use_translation();
    let editing = use_edit_mode();
    let (Some(ui), Some(context)) = (ui, context) else { return html! {} };
    let plugins: Vec<_> = ui.plugins.iter().filter(|plugin| plugin.config_section).collect();
    html! {
        <>
            if !plugins.is_empty() {
                <div class="tp__plugin-sections">
                    { for plugins.iter().map(|plugin| {
                        let key = format!("section:{}", plugin.id);
                        if !plugin_available(plugin) {
                            return html! { <PluginUnavailable key={key} plugin={Rc::clone(plugin)} title={String::new()} /> };
                        }
                        let Some(draft) = context.state.plugins.get(&plugin.id) else {
                            return html! { <div key={key}><PluginBadge name={plugin.name.clone()} /><PluginSettingsNotice id={plugin.id.clone()} /></div> };
                        };
                        let strings = ui.strings_for(&plugin.id);
                        let form = match draft.schema.as_ref() {
                            Some(schema) => {
                                let on_change = {
                                    let dispatch = context.dispatch.clone();
                                    let id = plugin.id.clone();
                                    Callback::from(move |value: Value| {
                                        dispatch.emit(PluginSettingsAction::EditMain { id: id.clone(), value });
                                    })
                                };
                                html! {
                                    <fieldset class="tp__plugin-ext__fields" disabled={!editing}>
                                        <SchemaForm
                                            key={draft.generation.to_string()}
                                            schema={translate_schema(schema, &strings)}
                                            value={main_value(&draft.draft)}
                                            secrets={main_secrets(&draft.secrets)}
                                            errors={main_errors(&draft.errors)}
                                            on_change={on_change}
                                        />
                                    </fieldset>
                                }
                            }
                            None => html! {
                                <p class="tp__plugin-settings__note">{ translate.t("MESSAGES.PLUGIN_SETTINGS_NO_SCHEMA") }</p>
                            },
                        };
                        html! {
                            <div key={key} class="tp__plugin-ext tp__plugin-section">
                                <div class="tp__plugin-ext__divider"><PluginBadge name={plugin.name.clone()} /></div>
                                { render_form_errors(draft) }
                                { form }
                            </div>
                        }
                    }) }
                </div>
            }
            <PluginSlot slot={PluginSlotDto::ConfigSection} />
        </>
    }
}

#[derive(Properties, PartialEq)]
pub struct PluginConfigTabProps {
    pub plugin: Rc<PluginUiDto>,
    /// The frame of a `config-tab` contribution; `None` renders `ui.config_tab`.
    #[prop_or_default]
    pub element: Option<String>,
}

/// The content of a plugin tab: its schema form, or its frame.
#[component]
pub fn PluginConfigTab(props: &PluginConfigTabProps) -> Html {
    let ui = use_plugin_ui();
    let context = use_context::<PluginSettingsContext>();
    let editing = use_edit_mode();
    let plugin = &props.plugin;
    let Some(ui) = ui else { return html! {} };
    if !plugin_available(plugin) {
        return html! { <PluginUnavailable plugin={Rc::clone(plugin)} title={String::new()} /> };
    }
    if let Some(element) = props.element.clone() {
        return html! { <PluginFrame plugin={Rc::clone(plugin)} element={element} slot={PluginSlotDto::ConfigTab} /> };
    }
    let strings = ui.strings_for(&plugin.id);
    let tab = plugin.config_tab.as_ref();
    if let Some(schema) = tab.and_then(|tab| tab.schema.as_ref()) {
        let Some(context) = context else { return html! {} };
        let Some(draft) = context.state.plugins.get(&plugin.id) else {
            return html! { <PluginSettingsNotice id={plugin.id.clone()} /> };
        };
        let on_change = {
            let dispatch = context.dispatch.clone();
            let id = plugin.id.clone();
            Callback::from(move |value: Value| {
                dispatch.emit(PluginSettingsAction::EditMain { id: id.clone(), value });
            })
        };
        return html! {
            <div class="tp__plugin-ext">
                { render_form_errors(draft) }
                <fieldset class="tp__plugin-ext__fields" disabled={!editing}>
                    <SchemaForm
                        key={draft.generation.to_string()}
                        schema={translate_schema(schema, &strings)}
                        value={main_value(&draft.draft)}
                        secrets={main_secrets(&draft.secrets)}
                        errors={main_errors(&draft.errors)}
                        on_change={on_change}
                    />
                </fieldset>
            </div>
        };
    }
    let element = tab.and_then(|tab| tab.element.clone());
    element.map_or_else(Html::default, |element| {
        html! {
            <PluginFrame plugin={Rc::clone(plugin)} element={element} slot={PluginSlotDto::ConfigTab} />
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The ids of the plugins with unsaved edits.
    fn modified_ids(state: &PluginSettingsState) -> Vec<String> {
        state.plugins.iter().filter(|(_, draft)| draft.is_modified()).map(|(id, _)| id.clone()).collect()
    }

    fn fragment() -> Value {
        json!({"type": "object", "properties": {
            "host": {"type": "string", "default": "localhost"},
            "port": {"type": "integer"}
        }})
    }

    fn error(path: &str, message: &str) -> PluginFieldErrorDto {
        PluginFieldErrorDto { path: path.to_string(), message: message.to_string() }
    }

    fn settings_dto(settings: Value) -> PluginSettingsDto {
        PluginSettingsDto { settings, schema: None, secrets: Vec::new(), warnings: Vec::new() }
    }

    fn loaded(state: PluginSettingsState, id: &str, settings: Value) -> Rc<PluginSettingsState> {
        Rc::new(state).reduce(PluginSettingsAction::Loaded {
            id: id.to_string(),
            settings: settings_dto(settings),
            extensions: vec![("proxy".to_string(), fragment())],
        })
    }

    #[test]
    fn sections_live_under_ext_and_do_not_touch_the_rest() {
        let settings = json!({"token": "abc", "ext": {"proxy": {"port": 1}, "log": {"level": "info"}}});
        assert_eq!(section_value(&settings, "proxy"), json!({"port": 1}));
        assert_eq!(section_value(&settings, "missing"), json!({}));
        let changed = with_section(&settings, "proxy", json!({"port": 2}));
        assert_eq!(changed["ext"]["proxy"], json!({"port": 2}));
        assert_eq!(changed["ext"]["log"], json!({"level": "info"}));
        assert_eq!(changed["token"], "abc");
        assert_eq!(main_value(&settings), json!({"token": "abc"}));
        assert_eq!(with_main(&settings, &json!({"token": "new"}))["ext"], settings["ext"]);
        assert_eq!(with_main(&settings, &json!({"token": "new", "ext": {"evil": {}}}))["ext"], settings["ext"]);
        assert_eq!(with_section(&Value::Null, "proxy", json!({}))["ext"]["proxy"], json!({}));
    }

    #[test]
    fn secrets_and_errors_are_split_between_the_fragment_and_the_plugin() {
        let secrets = vec![
            PluginSecretDto { path: "token".to_string(), set: true },
            PluginSecretDto { path: "ext.proxy.password".to_string(), set: false },
            PluginSecretDto { path: "ext.log.key".to_string(), set: true },
        ];
        assert_eq!(section_secrets(&secrets, "proxy"), [PluginSecretDto { path: "password".to_string(), set: false }]);
        assert_eq!(main_secrets(&secrets), [PluginSecretDto { path: "token".to_string(), set: true }]);

        let errors = vec![
            error("ext.proxy.port", "must be an integer"),
            error("ext.proxy", "missing fields"),
            error("ext.proxyx.port", "other section"),
            error("ext.log.level", "bad level"),
            error("token", "too short"),
            error("(root)", "bad"),
        ];
        assert_eq!(
            section_errors(&errors, "proxy"),
            [error("port", "must be an integer"), error("(root)", "missing fields")]
        );
        assert_eq!(main_errors(&errors), [error("token", "too short"), error("(root)", "bad")]);
    }

    #[test]
    fn the_baseline_fills_defaults_of_the_schema_and_of_every_fragment() {
        let schema = json!({"type": "object", "properties": {"mode": {"type": "string", "default": "fast"}}});
        let extensions = vec![("proxy".to_string(), fragment())];
        let base = baseline(&json!({"token": "x"}), Some(&schema), &extensions);
        assert_eq!(base, json!({"token": "x", "mode": "fast", "ext": {"proxy": {"host": "localhost"}}}));
        // A fragment without defaults leaves the settings as they were.
        let plain = vec![("proxy".to_string(), json!({"type": "object", "properties": {"port": {"type": "integer"}}}))];
        assert_eq!(baseline(&json!({"token": "x"}), None, &plain), json!({"token": "x"}));
        // Existing values win over defaults.
        assert_eq!(
            baseline(&json!({"ext": {"proxy": {"host": "10.0.0.1"}}}), None, &extensions)["ext"]["proxy"]["host"],
            "10.0.0.1"
        );
    }

    #[test]
    fn editing_marks_the_section_and_the_plugin_modified() {
        let state = loaded(PluginSettingsState::default(), "p", json!({"token": "x"}));
        assert!(!state.has_changes());
        let state = state.reduce(PluginSettingsAction::EditSection {
            id: "p".to_string(),
            section: "proxy".to_string(),
            value: json!({"host": "other"}),
        });
        assert!(state.has_changes());
        assert_eq!(modified_ids(&state), ["p"]);
        assert_eq!(state.modified_sections(), ["proxy"]);
        // Editing the plugin's own settings is a change, but not in any built-in section.
        let state = loaded(PluginSettingsState::default(), "p", json!({"token": "x"}))
            .reduce(PluginSettingsAction::EditMain { id: "p".to_string(), value: json!({"token": "y"}) });
        assert_eq!(modified_ids(&state), ["p"]);
        assert_eq!(state.modified_sections(), Vec::<String>::new());
        assert_eq!(state.plugins["p"].draft["ext"]["proxy"], json!({"host": "localhost"}), "ext survives an own edit");
    }

    #[test]
    fn reset_discards_edits_and_errors_and_rebuilds_the_forms() {
        let state = loaded(PluginSettingsState::default(), "p", json!({}))
            .reduce(PluginSettingsAction::EditSection {
                id: "p".to_string(),
                section: "proxy".to_string(),
                value: json!({"host": "other"}),
            })
            .reduce(PluginSettingsAction::Rejected {
                id: "p".to_string(),
                error: "no".to_string(),
                fields: vec![error("ext.proxy.host", "bad")],
            });
        assert_eq!(state.plugins["p"].errors.len(), 1);
        let generation = state.plugins["p"].generation;
        let state = state.reduce(PluginSettingsAction::Reset);
        assert!(!state.has_changes());
        assert!(state.plugins["p"].error.is_none() && state.plugins["p"].errors.is_empty());
        assert_eq!(state.plugins["p"].generation, generation + 1);
    }

    #[test]
    fn a_rejected_save_keeps_the_draft_and_shows_the_fields() {
        let state = loaded(PluginSettingsState::default(), "p", json!({})).reduce(PluginSettingsAction::EditSection {
            id: "p".to_string(),
            section: "proxy".to_string(),
            value: json!({"port": "x"}),
        });
        let state = state.reduce(PluginSettingsAction::Rejected {
            id: "p".to_string(),
            error: "settings do not match the schema".to_string(),
            fields: vec![error("ext.proxy.port", "must be an integer")],
        });
        let draft = &state.plugins["p"];
        assert!(draft.is_modified(), "nothing was saved");
        assert_eq!(section_errors(&draft.errors, "proxy"), [error("port", "must be an integer")]);
    }

    #[test]
    fn a_saved_plugin_is_clean_again_and_its_forms_are_rebuilt() {
        let state = loaded(PluginSettingsState::default(), "p", json!({})).reduce(PluginSettingsAction::EditSection {
            id: "p".to_string(),
            section: "proxy".to_string(),
            value: json!({"host": "other"}),
        });
        let generation = state.plugins["p"].generation;
        let state = state.reduce(PluginSettingsAction::Saved {
            id: "p".to_string(),
            settings: settings_dto(json!({"ext": {"proxy": {"host": "other"}}})),
        });
        assert!(!state.has_changes());
        assert_eq!(state.plugins["p"].generation, generation + 1);
        assert_eq!(state.plugins["p"].extensions.len(), 1, "the fragments are kept");
    }

    #[test]
    fn a_failed_load_is_remembered_and_cleared_by_a_later_load() {
        let state = Rc::new(PluginSettingsState::default())
            .reduce(PluginSettingsAction::LoadFailed { id: "p".to_string(), message: "boom".to_string() });
        assert_eq!(state.load_errors["p"], "boom");
        let state = state.reduce(PluginSettingsAction::Loaded {
            id: "p".to_string(),
            settings: settings_dto(json!({})),
            extensions: Vec::new(),
        });
        assert!(state.load_errors.is_empty());
    }
}
