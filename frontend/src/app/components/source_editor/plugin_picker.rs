//! The installed-plugin picker shared by `type: plugin` inputs and outputs.
//!
//! Both forms need the same thing: the id of an installed plugin that declares
//! one manifest hook. The candidates come from `GET /api/v1/plugins`, so the
//! picker never offers an id that cannot run and never hides one that is
//! already configured.

use crate::{
    app::components::{DropDownOption, DropDownSelection, Select},
    config_field_child,
    hooks::use_service_context,
    i18n::use_translation,
};
use shared::model::{PluginStateDto, PluginSummaryDto};
use std::rc::Rc;
use yew::{component, html, platform::spawn_local, use_effect_with, use_state, Callback, Html, Properties};

/// The manifest hook an input provider declares (`hooks: [input.fetch]`).
pub const INPUT_FETCH_HOOK: &str = "input.fetch";
/// The manifest hook an output provider declares (`hooks: [output.render]`).
pub const OUTPUT_RENDER_HOOK: &str = "output.render";

/// Whether a plugin input or output form shows the picker, given the plugin the
/// block was opened with. A block from a plugin's own tile, or one already
/// configured, names its plugin, so the form does not ask again; only the
/// generic Plugin tile adds a block with no plugin chosen.
pub fn needs_plugin_picker(opened_with: Option<&str>) -> bool { opened_with.is_none_or(str::is_empty) }

/// A plugin that is enabled and runs now, so `validate_plugin_refs` accepts an
/// input or output naming it.
fn is_runnable(plugin: &PluginSummaryDto) -> bool { plugin.enabled && plugin.state == PluginStateDto::Enabled }

/// The installed plugins that declare `hook` and can run.
fn providers_for<'a>(plugins: &'a [PluginSummaryDto], hook: &str) -> Vec<&'a PluginSummaryDto> {
    plugins
        .iter()
        .filter(|plugin| is_runnable(plugin) && plugin.hooks.iter().any(|declared| declared == hook))
        .collect()
}

fn plugin_label(plugin: &PluginSummaryDto) -> String {
    if plugin.name == plugin.id {
        plugin.name.clone()
    } else {
        format!("{} ({})", plugin.name, plugin.id)
    }
}

/// One option per runnable provider. A configured id the list does not offer —
/// disabled, failed, uninstalled, or the list request failed — stays selectable
/// instead of leaving the field empty or silently dropping the value.
fn plugin_options(plugins: &[PluginSummaryDto], hook: &str, selected: Option<&str>) -> Rc<Vec<DropDownOption>> {
    let mut options = providers_for(plugins, hook)
        .into_iter()
        .map(|plugin| {
            DropDownOption::new(&plugin.id, html! { {plugin_label(plugin)} }, selected == Some(plugin.id.as_str()))
        })
        .collect::<Vec<_>>();
    if let Some(id) = selected.filter(|id| !options.iter().any(|option| option.id == *id)) {
        options.push(DropDownOption::new(id, html! { {id} }, true));
    }
    Rc::new(options)
}

#[derive(Properties, Clone, PartialEq)]
pub struct PluginPickerProps {
    /// `name` and `aria-label` of the select.
    pub name: String,
    pub label: String,
    /// Field id the help dialog resolves the explanation from.
    pub field_id: String,
    /// Explanation key below the label; `FieldLabel` adds the `EXPLANATION.` prefix.
    #[prop_or_default]
    pub hint_key: Option<String>,
    /// The manifest hook a candidate must declare.
    pub hook: String,
    /// The configured plugin id, when the form has one.
    #[prop_or_default]
    pub selected: Option<String>,
    #[prop_or(true)]
    pub allow_write: bool,
    /// Already localized field error shown while nothing is selected.
    #[prop_or_default]
    pub required_error: Option<String>,
    /// Translation key of the note under the field, shown when no installed
    /// plugin declares `hook` and nothing is configured yet. The picker
    /// translates it; never pass translated text.
    #[prop_or_default]
    pub empty_hint_key: Option<String>,
    pub on_select: Callback<Option<String>>,
}

#[component]
pub fn PluginPicker(props: &PluginPickerProps) -> Html {
    let translate = use_translation();
    let services = use_service_context();
    let plugins = use_state(|| None::<Vec<PluginSummaryDto>>);

    {
        let plugins = plugins.clone();
        let allow_write = props.allow_write;
        use_effect_with(allow_write, move |allow_write| {
            if *allow_write {
                spawn_local(async move {
                    // A failed request leaves the list unknown, so the form claims nothing
                    // about which plugins are installed.
                    if let Ok(list) = services.plugins.list().await {
                        plugins.set(Some(list));
                    }
                });
            }
        });
    }

    let selected = props.selected.as_deref();
    let loaded = plugins.as_ref().map(Vec::as_slice);
    let candidates = loaded.map_or(0, |list| providers_for(list, &props.hook).len());
    let options = plugin_options(loaded.unwrap_or(&[]), &props.hook, selected);

    let field = if props.allow_write {
        let on_select = props.on_select.clone();
        let error = selected.is_none().then(|| props.required_error.clone()).flatten();
        html! {
            <Select name={props.name.clone()} multi_select={false} required={true} {options} {error}
                on_select={Callback::from(move |(_, selection): (String, DropDownSelection)| {
                    let id = match selection {
                        DropDownSelection::Single(id) => Some(id).filter(|id| !id.is_empty()),
                        DropDownSelection::Empty | DropDownSelection::Multi(_) => None,
                    };
                    on_select.emit(id);
                })} />
        }
    } else {
        html! { <span class="tp__form-field__value">{selected.unwrap_or_default()}</span> }
    };

    let empty_hint = if loaded.is_some() && candidates == 0 && selected.is_none() {
        props.empty_hint_key.as_ref().map(|key| html! { <span class="tp__form-field__value">{translate.t(key)}</span> })
    } else {
        None
    };

    html! {
        <>
            { config_field_child!(props.label.clone(), props.field_id.clone(), props.hint_key.clone(), { field }) }
            { empty_hint.unwrap_or_default() }
        </>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::PluginStateDto;

    fn plugin(id: &str, hooks: &[&str]) -> PluginSummaryDto {
        PluginSummaryDto {
            id: id.to_string(),
            name: id.to_string(),
            version: None,
            description: None,
            category: None,
            state: PluginStateDto::Enabled,
            failure: None,
            enabled: true,
            priority: 0,
            hooks: hooks.iter().map(|hook| (*hook).to_string()).collect(),
            capabilities: Vec::new(),
            abi: String::new(),
            wasm_sha256: None,
            settings_review: None,
            update: None,
            rollback: None,
            origin: None,
            update_policy: Default::default(),
        }
    }

    #[test]
    fn a_disabled_or_failed_plugin_is_not_offered() {
        let mut disabled = plugin("csv-writer", &["output.render"]);
        disabled.enabled = false;
        let mut failed = plugin("xml-writer", &["output.render"]);
        failed.state = PluginStateDto::Failed;
        let plugins = [disabled, failed];

        assert_eq!(plugin_options(&plugins, OUTPUT_RENDER_HOOK, None).len(), 0, "nothing can run");
        // A configured one stays listed so the value is not lost.
        let options = plugin_options(&plugins, OUTPUT_RENDER_HOOK, Some("csv-writer"));
        assert_eq!(options.len(), 1);
        assert_eq!(options[0].id, "csv-writer");
    }

    #[test]
    fn only_plugins_declaring_the_requested_hook_are_offered() {
        let plugins = [
            plugin("youtube-input", &["input.fetch"]),
            plugin("csv-writer", &["output.render"]),
            plugin("telegram-notifier", &["notification.send"]),
        ];

        let inputs = plugin_options(&plugins, INPUT_FETCH_HOOK, None);
        let outputs = plugin_options(&plugins, OUTPUT_RENDER_HOOK, None);

        assert_eq!(inputs.iter().map(|option| option.id.as_str()).collect::<Vec<_>>(), ["youtube-input"]);
        assert_eq!(outputs.iter().map(|option| option.id.as_str()).collect::<Vec<_>>(), ["csv-writer"]);
        assert!(!inputs[0].selected && !outputs[0].selected);
    }

    #[test]
    fn configured_plugin_stays_selectable_when_the_list_misses_it() {
        let plugins = [plugin("youtube-input", &["input.fetch"])];
        let options = plugin_options(&plugins, INPUT_FETCH_HOOK, Some("gone-input"));

        assert_eq!(options.len(), 2);
        assert_eq!(options[1].id, "gone-input");
        assert!(options[1].selected);
    }

    #[test]
    fn only_a_block_without_a_plugin_shows_the_picker() {
        assert!(needs_plugin_picker(None), "the generic Plugin tile");
        assert!(needs_plugin_picker(Some("")), "an empty id names no plugin");
        assert!(!needs_plugin_picker(Some("youtube-input")), "a plugin's own tile or a configured block");
    }

    #[test]
    fn selected_provider_is_marked() {
        let plugins = [plugin("csv-writer", &["output.render"])];
        let options = plugin_options(&plugins, OUTPUT_RENDER_HOOK, Some("csv-writer"));

        assert_eq!(options.len(), 1);
        assert!(options[0].selected);
    }

    #[test]
    fn the_short_label_is_used_when_name_and_id_match() {
        let mut named = plugin("csv-writer", &["output.render"]);
        named.name = "CSV Writer".to_string();

        assert_eq!(plugin_label(&plugin("csv-writer", &[])), "csv-writer");
        assert_eq!(plugin_label(&named), "CSV Writer (csv-writer)");
    }

    #[test]
    fn the_label_exists_in_every_locale() {
        for locale in [
            include_str!("../../../../public/assets/i18n/en.json"),
            include_str!("../../../../public/assets/i18n/es.json"),
            include_str!("../../../../public/assets/i18n/ar.json"),
            include_str!("../../../../public/assets/i18n/ru.json"),
        ] {
            let translations: serde_json::Value = serde_json::from_str(locale).unwrap();
            assert!(translations.pointer("/LABEL/PLUGIN").and_then(serde_json::Value::as_str).is_some());
        }
    }
}
