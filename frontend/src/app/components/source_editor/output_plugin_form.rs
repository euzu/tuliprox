//! The form of a `type: plugin` target output.
//!
//! The host owns the fields every output has: the plugin that renders it, the
//! file name in the target's storage directory, and the filter that selects the
//! items. The plugin owns the rest: its `ui.output_form` sections render into
//! the Main and Options panels, or into panels of their own, over the output's
//! `plugin_config` (handed to the plugin as `output-request.options`).
//!
//! `format` is not asked for. It only has to equal the plugin's
//! `provides.output_format`, so picking another plugin clears it and the host
//! uses the plugin's own format.

use crate::{
    app::components::{
        config::HasFormData,
        source_editor::plugin_form::{
            has_options_section, host_section_title, plugin_sections, replaces_plugin, PluginFormKind, SECTION_MAIN,
            SECTION_OPTIONS,
        },
        use_plugin_ui, BlockId, BlockInstance, Card, EditMode, FilterInput, IconButton, Panel, PluginPicker,
        PluginSection, PluginSectionCard, SourceEditorContext, TextButton, OUTPUT_RENDER_HOOK,
    },
    config_field_child, edit_field_text_option, generate_form_reducer,
    i18n::use_translation,
};
use serde_json::{Map, Value};
use shared::model::{PluginTargetOutputDto, TargetOutputDto};
use std::{rc::Rc, sync::Arc};
use yew::{
    component, html, use_context, use_effect_with, use_reducer, use_state, Callback, Html, Properties, UseReducerHandle,
};

const LABEL_PLUGIN: &str = "LABEL.PLUGIN";
const LABEL_FILENAME: &str = "LABEL.FILENAME";
const LABEL_FILTER: &str = "LABEL.FILTER";

const HINT_PLUGIN: &str = "OUTPUT_PLUGIN_FORM.PLUGIN";
const HINT_PLUGIN_NONE: &str = "EXPLANATION.OUTPUT_PLUGIN_FORM.PLUGIN_NONE";
const HINT_PLUGIN_REQUIRED: &str = "EXPLANATION.OUTPUT_PLUGIN_FORM.PLUGIN_REQUIRED";
const HINT_FILENAME: &str = "OUTPUT_PLUGIN_FORM.FILENAME";
const HINT_FILTER: &str = "OUTPUT_PLUGIN_FORM.FILTER";

generate_form_reducer!(
    state: PluginTargetOutputFormState { form: PluginTargetOutputDto },
    action_name: PluginTargetOutputFormAction,
    fields {
        Plugin => plugin: String,
        Format => format: Option<String>,
        Filename => filename: Option<String>,
        PluginConfig => plugin_config: Map<String, Value>,
        Filter => filter: Option<String>,
    }
);

#[derive(Properties, PartialEq, Clone)]
pub struct PluginTargetOutputViewProps {
    pub(crate) block_id: BlockId,
    pub(crate) output: Option<Rc<PluginTargetOutputDto>>,
    #[prop_or(true)]
    pub(crate) allow_write: bool,
}

#[component]
pub fn PluginTargetOutputView(props: &PluginTargetOutputViewProps) -> Html {
    let translate = use_translation();
    let ui = use_plugin_ui();
    let source_editor_ctx = use_context::<SourceEditorContext>().expect("SourceEditorContext not found");

    let state: UseReducerHandle<PluginTargetOutputFormState> =
        use_reducer(|| PluginTargetOutputFormState { form: PluginTargetOutputDto::default(), modified: false });
    let active_panel = use_state(|| Arc::<str>::from(SECTION_MAIN));

    {
        let state = state.clone();
        let output = props.output.clone();
        use_effect_with(output, move |output| {
            let form = output.as_ref().map_or_else(PluginTargetOutputDto::default, |output| output.as_ref().clone());
            state.dispatch(PluginTargetOutputFormAction::SetAll(form));
            || ()
        });
    }

    let form = state.form.clone();
    let plugin_id = form.plugin.clone();
    let host_title = |id: &str| host_section_title(id, &translate);
    let sections: Vec<PluginSection> = plugin_sections(ui.as_ref(), &plugin_id, PluginFormKind::Output, &host_title);
    let config_value = Value::Object(form.plugin_config.clone());

    let on_config_change = {
        let state = state.clone();
        Callback::from(move |value: Value| {
            let map = match value {
                Value::Object(map) => map,
                _ => Map::new(),
            };
            state.dispatch(PluginTargetOutputFormAction::PluginConfig(map));
        })
    };

    let on_plugin_select = {
        let state = state.clone();
        let active_panel = active_panel.clone();
        Callback::from(move |id: Option<String>| {
            // The list emits on every click, the selected one included: re-picking the same plugin
            // must keep its values and its format.
            if !replaces_plugin(&state.data().plugin, id.as_deref()) {
                return;
            }
            let id = id.unwrap_or_default();
            // Another plugin declares other fields, and `format` belonged to the old one.
            state.dispatch(PluginTargetOutputFormAction::PluginConfig(Map::new()));
            state.dispatch(PluginTargetOutputFormAction::Format(None));
            state.dispatch(PluginTargetOutputFormAction::Plugin(id));
            active_panel.set(Arc::from(SECTION_MAIN));
        })
    };

    let on_filter_change = {
        let state = state.clone();
        Callback::from(move |filter: Option<String>| {
            state.dispatch(PluginTargetOutputFormAction::Filter(filter));
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
            <PluginPicker name="plugin" label={translate.t(LABEL_PLUGIN)} field_id="OUTPUT_PLUGIN_FORM.PLUGIN_ID"
                hint_key={Some(HINT_PLUGIN.to_string())}
                hook={OUTPUT_RENDER_HOOK.to_string()}
                selected={(!plugin_id.is_empty()).then(|| plugin_id.clone())}
                allow_write={props.allow_write}
                required_error={Some(translate.t(HINT_PLUGIN_REQUIRED))}
                empty_hint_key={Some(HINT_PLUGIN_NONE.to_string())}
                on_select={on_plugin_select} />
            { edit_field_text_option!(state, translate.t(LABEL_FILENAME), filename, PluginTargetOutputFormAction::Filename, false, Some(HINT_FILENAME.to_string())) }
            { config_field_child!(translate.t(LABEL_FILTER), "OUTPUT_PLUGIN_FORM.FILTER",
                Some(HINT_FILTER.to_string()), {
                    html! {
                        <FilterInput filter={form.filter.clone()} on_change={on_filter_change.clone()} />
                    }
                }) }
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
        let ctx = source_editor_ctx.clone();
        let block_id = props.block_id;
        Callback::from(move |_| {
            let output = state.data().clone();
            ctx.on_form_change.emit((block_id, BlockInstance::Output(Rc::new(TargetOutputDto::Plugin(output)))));
            ctx.edit_mode.set(EditMode::Inactive);
        })
    };

    let handle_cancel = {
        let ctx = source_editor_ctx.clone();
        Callback::from(move |_| {
            ctx.edit_mode.set(EditMode::Inactive);
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
                <TextButton class="secondary" name="cancel_plugin_output" icon="Cancel"
                    title={translate.t("LABEL.CANCEL")} onclick={handle_cancel}></TextButton>
                if props.allow_write {
                    <TextButton class="primary" name="apply_plugin_output" icon="Accept"
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
            for key in [LABEL_PLUGIN, LABEL_FILENAME, LABEL_FILTER] {
                let pointer = format!("/{}", key.replace('.', "/"));
                assert!(translations.pointer(&pointer).and_then(serde_json::Value::as_str).is_some(), "{pointer}");
            }
            // `config_field_child!` resolves its hint key with the `EXPLANATION.` prefix; the two
            // picker messages are looked up whole and carry the prefix themselves.
            for key in [HINT_PLUGIN, HINT_FILENAME, HINT_FILTER] {
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
    fn a_saved_plugin_output_round_trips_through_the_form_state() {
        let output = PluginTargetOutputDto {
            plugin: "csv-writer".to_string(),
            format: Some("csv".to_string()),
            filename: Some("channels.csv".to_string()),
            plugin_config: Map::from_iter([
                ("delimiter".to_string(), Value::String(";".to_string())),
                ("header".to_string(), Value::Bool(true)),
            ]),
            filter: Some("Group ~ \".*\"".to_string()),
            t_filter: None,
        };

        let saved = serde_json::to_value(TargetOutputDto::Plugin(output.clone())).unwrap();
        let restored = serde_json::from_value::<TargetOutputDto>(saved).unwrap();

        assert_eq!(restored, TargetOutputDto::Plugin(output));
    }

    #[test]
    fn picking_another_plugin_drops_the_old_format_and_config() {
        let mut form = PluginTargetOutputDto {
            plugin: "csv-writer".to_string(),
            format: Some("csv".to_string()),
            plugin_config: Map::from_iter([("delimiter".to_string(), Value::String(";".to_string()))]),
            ..PluginTargetOutputDto::default()
        };

        // What the picker callback does on a change.
        form.plugin_config = Map::new();
        form.format = None;
        form.plugin = "xml-writer".to_string();

        assert_eq!(form.format, None);
        assert!(form.plugin_config.is_empty());
        assert_eq!(form.plugin, "xml-writer");
    }
}
