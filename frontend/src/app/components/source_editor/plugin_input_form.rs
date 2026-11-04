//! The form of a `type: plugin` input.
//!
//! The host owns the fields every input has — the plugin it runs on, its name,
//! whether it is enabled, its processing order and connection limits — and the
//! plugin owns everything else. Its `ui.input_form` sections (see
//! [`crate::app::components::source_editor::plugin_form`]) render into the Main
//! and Options panels, or into panels of their own, over the input's
//! `plugin_config`.
//!
//! The built-in downloader fields (headers, EPG sources, aliases, provider
//! profiles, staged providers, probe options) are not shown: a plugin fetches
//! its own data, and `ConfigInputDto::prepare` refuses most of them for a plugin
//! input.

use crate::{
    app::components::{
        config::HasFormData,
        needs_plugin_picker,
        source_editor::plugin_form::{
            has_options_section, host_section_title, plugin_sections, replaces_plugin, PluginFormKind, SECTION_MAIN,
            SECTION_OPTIONS,
        },
        use_plugin_ui, BlockId, BlockInstance, Card, EditMode, IconButton, Panel, PluginPicker, PluginSection,
        PluginSectionCard, SourceEditorContext, TextButton, INPUT_FETCH_HOOK,
    },
    edit_field_bool, edit_field_number_i16, edit_field_number_option_u32, edit_field_number_u16, edit_field_text,
    edit_field_text_option, generate_form_reducer,
    i18n::use_translation,
};
use serde_json::{Map, Value};
use shared::model::ConfigInputDto;
use std::{rc::Rc, sync::Arc};
use yew::{
    component, html, use_context, use_effect_with, use_reducer, use_state, Callback, Html, Properties, UseReducerHandle,
};

const LABEL_PLUGIN: &str = "LABEL.PLUGIN";
const LABEL_NAME: &str = "LABEL.NAME";
const LABEL_ENABLED: &str = "LABEL.ENABLED";
const LABEL_SEQUENTIAL_GROUP: &str = "LABEL.SEQUENTIAL_GROUP";
const LABEL_MAX_CONNECTIONS: &str = "LABEL.MAX_CONNECTIONS";
const LABEL_PRIORITY: &str = "LABEL.PRIORITY";
const LABEL_CACHE_DURATION: &str = "LABEL.CACHE_DURATION";
const LABEL_URL: &str = "LABEL.URL";
const LABEL_PLAYBACK_PROVIDER: &str = "LABEL.PLAYBACK_PROVIDER";

const HINT_PLUGIN: &str = "INPUT_FORM.PLUGIN";
const HINT_PLUGIN_NONE: &str = "EXPLANATION.INPUT_FORM.PLUGIN_NONE";
const HINT_PLUGIN_REQUIRED: &str = "EXPLANATION.INPUT_FORM.PLUGIN_REQUIRED";
const HINT_URL: &str = "INPUT_FORM.PLUGIN_URL";
const HINT_PLAYBACK_PROVIDER: &str = "INPUT_FORM.PLAYBACK_PROVIDER";

generate_form_reducer!(
    state: PluginInputFormState { form: ConfigInputDto },
    action_name: PluginInputFormAction,
    fields {
        Name => name: String,
        Url => url: String,
        Plugin => plugin: Option<Arc<str>>,
        Enabled => enabled: bool,
        SequentialGroup => sequential_group: Option<u32>,
        MaxConnections => max_connections: u16,
        Priority => priority: i16,
        CacheDuration => cache_duration: Option<String>,
        PlaybackProvider => playback_provider: Option<String>,
        PluginConfig => plugin_config: Map<String, Value>,
    }
);

#[derive(Properties, PartialEq, Clone)]
pub struct PluginInputViewProps {
    #[prop_or_default]
    pub block_id: Option<BlockId>,
    pub input: Option<Rc<ConfigInputDto>>,
    #[prop_or(true)]
    pub allow_write: bool,
    #[prop_or_default]
    pub on_apply: Option<Callback<ConfigInputDto>>,
}

#[component]
pub fn PluginInputView(props: &PluginInputViewProps) -> Html {
    let translate = use_translation();
    let ui = use_plugin_ui();
    let source_editor_ctx = use_context::<SourceEditorContext>();

    let state: UseReducerHandle<PluginInputFormState> =
        use_reducer(|| PluginInputFormState { form: ConfigInputDto::default(), modified: false });
    let active_panel = use_state(|| Arc::<str>::from(SECTION_MAIN));

    {
        let state = state.clone();
        let input = props.input.clone();
        use_effect_with(input, move |input| {
            let mut form = input.as_ref().map_or_else(ConfigInputDto::default, |input| input.as_ref().clone());
            if form.plugin_config.is_empty() {
                form.plugin_config = Map::new();
            }
            state.dispatch(PluginInputFormAction::SetAll(form));
            || ()
        });
    }

    let form = state.form.clone();
    let picks_plugin = needs_plugin_picker(props.input.as_ref().and_then(|input| input.plugin.as_deref()));
    let host_title = |id: &str| host_section_title(id, &translate);
    let plugin_id = form.plugin.as_deref().unwrap_or_default().to_string();
    let sections: Vec<PluginSection> = plugin_sections(ui.as_ref(), &plugin_id, PluginFormKind::Input, &host_title);
    let config_value = Value::Object(form.plugin_config.clone());

    let on_config_change = {
        let state = state.clone();
        Callback::from(move |value: Value| {
            let map = match value {
                Value::Object(map) => map,
                _ => Map::new(),
            };
            state.dispatch(PluginInputFormAction::PluginConfig(map));
        })
    };

    let on_plugin_select = {
        let state = state.clone();
        let active_panel = active_panel.clone();
        Callback::from(move |id: Option<String>| {
            // The list emits on every click, the selected one included: re-picking the same plugin
            // must keep its values.
            if !replaces_plugin(state.data().plugin.as_deref().unwrap_or_default(), id.as_deref()) {
                return;
            }
            // Another plugin declares other fields; its values do not carry over.
            state.dispatch(PluginInputFormAction::PluginConfig(Map::new()));
            state.dispatch(PluginInputFormAction::Plugin(id.map(Arc::from)));
            active_panel.set(Arc::from(SECTION_MAIN));
        })
    };

    let cards_for = |panel: &str| -> Html {
        super::plugin_form::sections_for_panel(&sections, panel)
            .into_iter()
            .map(|section| {
                html! {
                    <PluginSectionCard key={section.id.clone()} section={section.clone()}
                        value={config_value.clone()} allow_write={props.allow_write}
                        on_change={on_config_change.clone()} />
                }
            })
            .collect::<Html>()
    };

    // Read-only access still shows the values, but nothing in them can be edited.
    let main_panel = html! {
        <Card class="tp__config-view__card">
        <fieldset class="tp__plugin-ext__fields" disabled={!props.allow_write}>
            if picks_plugin {
                <PluginPicker name="plugin" label={translate.t(LABEL_PLUGIN)} field_id="INPUT_FORM.PLUGIN_ID"
                    hint_key={Some(HINT_PLUGIN.to_string())}
                    hook={INPUT_FETCH_HOOK.to_string()}
                    selected={(!plugin_id.is_empty()).then(|| plugin_id.clone())}
                    allow_write={props.allow_write}
                    required_error={Some(translate.t(HINT_PLUGIN_REQUIRED))}
                    empty_hint_key={Some(HINT_PLUGIN_NONE.to_string())}
                    on_select={on_plugin_select} />
            }
            <div class="tp__config-view__cols-2">
                { edit_field_text!(state, translate.t(LABEL_NAME), name, PluginInputFormAction::Name) }
                { edit_field_bool!(state, translate.t(LABEL_ENABLED), enabled, PluginInputFormAction::Enabled) }
            </div>
            <div class="tp__config-view__cols-2">
                { edit_field_number_option_u32!(state, translate.t(LABEL_SEQUENTIAL_GROUP), sequential_group, PluginInputFormAction::SequentialGroup) }
                { edit_field_number_u16!(state, translate.t(LABEL_MAX_CONNECTIONS), max_connections, PluginInputFormAction::MaxConnections) }
            </div>
            <div class="tp__config-view__cols-2">
                { edit_field_number_i16!(state, translate.t(LABEL_PRIORITY), priority, PluginInputFormAction::Priority) }
                { edit_field_text_option!(state, translate.t(LABEL_CACHE_DURATION), cache_duration, PluginInputFormAction::CacheDuration) }
            </div>
            { edit_field_text!(state, translate.t(LABEL_URL), url, PluginInputFormAction::Url, false, Some(HINT_URL.to_string())) }
            { edit_field_text_option!(state, translate.t(LABEL_PLAYBACK_PROVIDER), playback_provider, PluginInputFormAction::PlaybackProvider, false, Some(HINT_PLAYBACK_PROVIDER.to_string())) }
            { cards_for(SECTION_MAIN) }
        </fieldset>
        </Card>
    };

    let panels = {
        let mut panels = vec![(SECTION_MAIN.to_string(), translate.t("LABEL.MAIN"))];
        if has_options_section(&sections) {
            panels.push((SECTION_OPTIONS.to_string(), translate.t("LABEL.OPTIONS")));
        }
        for section in &sections {
            if section.id != SECTION_MAIN && section.id != SECTION_OPTIONS {
                panels.push((section.id.clone(), section.title.clone()));
            }
        }
        panels
    };

    // A panel can disappear (another block, another plugin, a reloaded UI list). Showing the Main
    // panel beats an empty body with no highlighted entry.
    {
        let active_panel = active_panel.clone();
        let available = panels.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>().join(",");
        use_effect_with(available, move |available| {
            if !available.split(',').any(|id| id == active_panel.as_ref()) {
                active_panel.set(Arc::from(SECTION_MAIN));
            }
            || ()
        });
    }

    let handle_apply = {
        let state = state.clone();
        let on_apply = props.on_apply.clone();
        let block_id = props.block_id;
        let ctx = source_editor_ctx.clone();
        Callback::from(move |_| {
            let mut input = state.data().clone();
            input.input_type = shared::model::InputType::Plugin;
            input.staged = None;
            input.media_server = None;
            input.stalker = None;
            if let Some(on_apply) = &on_apply {
                on_apply.emit(input);
            } else if let (Some(ctx), Some(block_id)) = (&ctx, block_id) {
                ctx.on_form_change.emit((block_id, BlockInstance::Input(Rc::new(input))));
                ctx.edit_mode.set(EditMode::Inactive);
            }
        })
    };

    let handle_cancel = {
        let ctx = source_editor_ctx.clone();
        Callback::from(move |_| {
            if let Some(ctx) = &ctx {
                ctx.edit_mode.set(EditMode::Inactive);
            }
        })
    };

    let on_panel_click = {
        let active_panel = active_panel.clone();
        Callback::from(move |(name, _): (String, yew::MouseEvent)| {
            active_panel.set(Arc::from(name.as_str()));
        })
    };

    html! {
        <div class="tp__source-editor-form tp__config-view-page">
            <div class="tp__source-editor-form__toolbar tp__form-page__toolbar">
                <TextButton class="secondary" name="cancel_plugin_input" icon="Cancel"
                    title={translate.t("LABEL.CANCEL")} onclick={handle_cancel}></TextButton>
                if props.allow_write {
                    <TextButton class="primary" name="apply_plugin_input" icon="Accept"
                        title={translate.t("LABEL.OK")} onclick={handle_apply}></TextButton>
                }
            </div>
            <div class="tp__source-editor-form__content">
                <div class="tp__source-editor-form__sidebar">
                    { for panels.iter().map(|(id, title)| html! {
                        <IconButton class={format!("tp__app-sidebar-menu--{id}{}", if active_panel.as_ref() == id.as_str() { " active" } else { "" })}
                            icon={if id == SECTION_MAIN { "Settings" } else { "Options" }}
                            hint={title.clone()} name={id.clone()} onclick={on_panel_click.clone()}></IconButton>
                    }) }
                </div>
                <div class="tp__source-editor-form__body">
                    <div class="tp__source-editor-form__body__pages">
                        { for panels.iter().map(|(id, _)| html! {
                            <Panel value={Arc::from(id.as_str())} active={(*active_panel).clone()}>
                                if id == SECTION_MAIN {
                                    { main_panel.clone() }
                                } else if id == SECTION_OPTIONS {
                                    <Card class="tp__config-view__card">{ cards_for(SECTION_OPTIONS) }</Card>
                                } else {
                                    { cards_for(id) }
                                }
                            </Panel>
                        }) }
                    </div>
                </div>
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_form_keys_exist_in_every_locale() {
        for locale in [
            include_str!("../../../../public/assets/i18n/en.json"),
            include_str!("../../../../public/assets/i18n/es.json"),
            include_str!("../../../../public/assets/i18n/ar.json"),
            include_str!("../../../../public/assets/i18n/ru.json"),
        ] {
            let translations: serde_json::Value = serde_json::from_str(locale).unwrap();
            for key in [
                LABEL_PLUGIN,
                LABEL_NAME,
                LABEL_ENABLED,
                LABEL_SEQUENTIAL_GROUP,
                LABEL_MAX_CONNECTIONS,
                LABEL_PRIORITY,
                LABEL_CACHE_DURATION,
                LABEL_URL,
                LABEL_PLAYBACK_PROVIDER,
            ] {
                let pointer = format!("/{}", key.replace('.', "/"));
                assert!(translations.pointer(&pointer).and_then(serde_json::Value::as_str).is_some(), "{pointer}");
            }
            // `FieldLabel` resolves its hint key with the `EXPLANATION.` prefix; the two
            // picker messages are looked up whole and carry the prefix themselves.
            for key in [HINT_PLUGIN, HINT_URL, HINT_PLAYBACK_PROVIDER] {
                let pointer = format!("/EXPLANATION/{}", key.replace('.', "/"));
                assert!(translations.pointer(&pointer).and_then(serde_json::Value::as_str).is_some(), "{pointer}");
            }
            for key in [HINT_PLUGIN_NONE, HINT_PLUGIN_REQUIRED] {
                let pointer = format!("/{}", key.replace('.', "/"));
                assert!(translations.pointer(&pointer).and_then(serde_json::Value::as_str).is_some(), "{pointer}");
            }
        }
    }

    #[test]
    fn applying_keeps_the_plugin_type_and_the_config() {
        let mut state = PluginInputFormState { form: ConfigInputDto::default(), modified: false };
        state.form.input_type = shared::model::InputType::M3u;
        state.form.plugin = Some(Arc::from("youtube-input"));
        state.form.plugin_config = Map::from_iter([("mode".to_string(), Value::String("live_only".to_string()))]);

        // The apply step resets the type after a form started on another one.
        let mut input = state.data().clone();
        input.input_type = shared::model::InputType::Plugin;
        input.staged = None;
        input.media_server = None;
        input.stalker = None;

        assert_eq!(input.input_type, shared::model::InputType::Plugin);
        assert_eq!(input.plugin.as_deref(), Some("youtube-input"));
        assert_eq!(input.plugin_config["mode"], Value::String("live_only".to_string()));
    }
}
