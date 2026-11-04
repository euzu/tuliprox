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
//! * Any other id adds a panel of its own, after them.

use crate::{
    app::components::{translate_schema, Card, PluginUiContext, SchemaForm},
    i18n::YewI18n,
};
use serde_json::Value;
use shared::model::PluginFormSectionDto;
use std::collections::BTreeMap;
use yew::{component, html, Callback, Html, Properties};

/// Which form of a plugin a section belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginFormKind {
    Input,
    Output,
}

/// A section with this id is shown inside the host's Main panel.
pub const SECTION_MAIN: &str = "main";
/// A section with this id is shown inside the host's Options panel.
pub const SECTION_OPTIONS: &str = "options";

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
}

impl PluginSection {
    fn new(id: String, title: String, schema: Value) -> Self {
        let in_host_panel = id == SECTION_MAIN || id == SECTION_OPTIONS;
        Self { id, title, in_host_panel, schema }
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
    sections
        .iter()
        .map(|section| {
            PluginSection::new(
                section.id.clone(),
                section_title(section, &strings, host_title),
                translate_schema(&section.schema, &strings),
            )
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

/// Whether the plugin adds a section to the host's Options panel. Without one
/// there is nothing an Options panel could show, since the built-in input
/// options belong to the built-in downloaders.
#[must_use]
pub fn has_options_section(sections: &[PluginSection]) -> bool {
    sections.iter().any(|section| section.id == SECTION_OPTIONS)
}

#[derive(Properties, Clone, PartialEq)]
pub struct PluginSectionCardProps {
    pub section: PluginSection,
    /// The whole `plugin_config` object; the section edits its own keys.
    pub value: Value,
    pub allow_write: bool,
    pub on_change: Callback<Value>,
}

/// One section's fields. A host-panel section renders bare, so it sits inside the
/// host's card; a panel of the plugin's own gets a card with its title.
#[component]
pub fn PluginSectionCard(props: &PluginSectionCardProps) -> Html {
    let on_change = props.on_change.clone();
    let fields = html! {
        <fieldset class="tp__plugin-ext__fields" disabled={!props.allow_write}>
            <SchemaForm schema={props.section.schema.clone()} value={props.value.clone()} {on_change} />
        </fieldset>
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn section(id: &str, title: Option<&str>, title_key: Option<&str>) -> PluginFormSectionDto {
        PluginFormSectionDto {
            id: id.to_string(),
            title: title.map(ToString::to_string),
            title_key: title_key.map(ToString::to_string),
            schema: json!({"type": "object", "properties": {"target": {"type": "string"}}}),
        }
    }

    fn host_title(id: &str) -> Option<String> { (id == SECTION_MAIN).then(|| "Main".to_string()) }

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
        assert!(has_options_section(&sections));
        assert!(sections[0].in_host_panel && sections[2].in_host_panel, "host panels carry no title of their own");
        assert!(!sections[1].in_host_panel, "a plugin panel keeps its title card");
    }
}
