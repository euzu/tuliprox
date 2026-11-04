//! What the source editor shows of input and output provider plugins: one
//! sidebar tile per runnable provider, the plugin's name on a block, and why a
//! block's plugin cannot run.
//!
//! Everything here reads the [`PluginUiContext`] (`GET /api/v1/plugins/ui`),
//! which lists every provider with its `input_tile` / `output_tile`. While that
//! list is not loaded — or never will be, for a user who may not read the
//! system — nothing is claimed about which plugins are installed.

use crate::{
    app::components::{
        plugin_available, title_or_key, unavailable_reason, Block, BlockInstance, BlockType, PluginUiContext,
    },
    i18n::YewI18n,
};
use shared::model::{PluginSourceTileDto, PluginStateDto, PluginUiDto, TargetOutputDto};
use std::rc::Rc;

const ISSUE_NOT_CHOSEN: &str = "SOURCE_EDITOR.PLUGIN_ISSUE_NOT_CHOSEN";
const ISSUE_MISSING: &str = "SOURCE_EDITOR.PLUGIN_ISSUE_MISSING";
const ISSUE_UNAVAILABLE: &str = "SOURCE_EDITOR.PLUGIN_ISSUE_UNAVAILABLE";
const ISSUE_NOT_INPUT: &str = "SOURCE_EDITOR.PLUGIN_ISSUE_NOT_INPUT_PROVIDER";
const ISSUE_NOT_OUTPUT: &str = "SOURCE_EDITOR.PLUGIN_ISSUE_NOT_OUTPUT_PROVIDER";
const NONE_INPUT: &str = "SOURCE_EDITOR.PLUGIN_TILE_NONE_INPUT";
const NONE_OUTPUT: &str = "SOURCE_EDITOR.PLUGIN_TILE_NONE_OUTPUT";

/// Every translation key this module uses; the locale test checks them.
#[cfg(test)]
pub(crate) const PLUGIN_TILE_KEYS: [&str; 7] =
    [ISSUE_NOT_CHOSEN, ISSUE_MISSING, ISSUE_UNAVAILABLE, ISSUE_NOT_INPUT, ISSUE_NOT_OUTPUT, NONE_INPUT, NONE_OUTPUT];

/// Which side of the source graph a provider serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PluginTileKind {
    /// `input.fetch`: a `type: plugin` input.
    Input,
    /// `output.render`: a `type: plugin` target output.
    Output,
}

impl PluginTileKind {
    pub const fn block_type(self) -> BlockType {
        match self {
            Self::Input => BlockType::InputPlugin,
            Self::Output => BlockType::OutputPlugin,
        }
    }

    pub const fn of(block_type: BlockType) -> Option<Self> {
        match block_type {
            BlockType::InputPlugin => Some(Self::Input),
            BlockType::OutputPlugin => Some(Self::Output),
            _ => None,
        }
    }

    fn tile(self, plugin: &PluginUiDto) -> Option<&PluginSourceTileDto> {
        match self {
            Self::Input => plugin.input_tile.as_ref(),
            Self::Output => plugin.output_tile.as_ref(),
        }
    }

    /// The translation key of the note on the greyed-out tile shown when no
    /// provider is installed.
    pub const fn none_installed_key(self) -> &'static str {
        match self {
            Self::Input => NONE_INPUT,
            Self::Output => NONE_OUTPUT,
        }
    }
}

/// One sidebar tile of a provider plugin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginBrick {
    pub plugin_id: String,
    /// Already localized.
    pub title: String,
    /// Already localized tooltip.
    pub description: Option<String>,
    /// Icon name as the manifest gave it; the sidebar renders it only when the
    /// app's icon set has it.
    pub icon: Option<String>,
}

/// The tiles of the enabled providers of `kind`, by title. A disabled or
/// failed provider gets no tile: an input added from it could not run.
pub fn plugin_bricks(ui: &PluginUiContext, kind: PluginTileKind) -> Vec<PluginBrick> {
    let mut bricks = ui
        .plugins
        .iter()
        .filter(|plugin| plugin_available(plugin))
        .filter_map(|plugin| {
            let tile = kind.tile(plugin)?;
            let strings = ui.strings_for(&plugin.id);
            let description = tile
                .description_key
                .as_deref()
                .and_then(|key| strings.get(key).cloned())
                .or_else(|| tile.description.clone())
                .filter(|text| !text.trim().is_empty());
            Some(PluginBrick {
                plugin_id: plugin.id.clone(),
                title: title_or_key(&strings, &tile.title, tile.title_key.as_deref()),
                description,
                icon: tile.icon.clone(),
            })
        })
        .collect::<Vec<_>>();
    bricks.sort_by(|a, b| {
        a.title.to_lowercase().cmp(&b.title.to_lowercase()).then_with(|| a.plugin_id.cmp(&b.plugin_id))
    });
    bricks
}

/// The id of the plugin a plugin block runs on; `None` for any other block.
/// An empty string is a plugin block with no plugin chosen.
pub fn block_plugin_id(block: &Block) -> Option<&str> {
    match &block.instance {
        BlockInstance::Input(input) if block.block_type == BlockType::InputPlugin => {
            Some(input.plugin.as_deref().unwrap_or_default())
        }
        BlockInstance::Output(output) if block.block_type == BlockType::OutputPlugin => match output.as_ref() {
            TargetOutputDto::Plugin(dto) => Some(dto.plugin.as_str()),
            _ => None,
        },
        _ => None,
    }
}

/// What a plugin block shows for its plugin: the tile title, else the plugin
/// name, else — for a plugin the list does not have — its id.
pub fn plugin_block_label(ui: Option<&PluginUiContext>, kind: PluginTileKind, plugin_id: &str) -> Option<String> {
    let plugin_id = plugin_id.trim();
    if plugin_id.is_empty() {
        return None;
    }
    let found = ui.and_then(|ui| ui.plugin(plugin_id).map(|plugin| (ui, plugin)));
    Some(match found {
        Some((ui, plugin)) => match kind.tile(&plugin) {
            Some(tile) => title_or_key(&ui.strings_for(plugin_id), &tile.title, tile.title_key.as_deref()),
            None => plugin.name.clone(),
        },
        None => plugin_id.to_string(),
    })
}

/// Why a plugin block cannot run.
#[derive(Clone, Debug, PartialEq)]
pub enum PluginRefIssue {
    /// The block names no plugin.
    NotChosen,
    /// The plugin is not in the list: not installed, or not loaded.
    Missing(String),
    /// Listed, but disabled or failed.
    Unavailable(Rc<PluginUiDto>),
    /// Enabled, but it does not serve this side of the graph.
    NotProvider(Rc<PluginUiDto>),
}

/// The issue of a plugin block naming `plugin_id`, if it has one. Only a
/// missing choice is reported before the plugin list is loaded.
pub fn plugin_ref_issue(ui: Option<&PluginUiContext>, kind: PluginTileKind, plugin_id: &str) -> Option<PluginRefIssue> {
    let plugin_id = plugin_id.trim();
    if plugin_id.is_empty() {
        return Some(PluginRefIssue::NotChosen);
    }
    let ui = ui.filter(|ui| ui.loaded)?;
    let Some(plugin) = ui.plugin(plugin_id) else {
        return Some(PluginRefIssue::Missing(plugin_id.to_string()));
    };
    if plugin.state != PluginStateDto::Enabled {
        return Some(PluginRefIssue::Unavailable(plugin));
    }
    if kind.tile(&plugin).is_none() {
        return Some(PluginRefIssue::NotProvider(plugin));
    }
    None
}

/// The sentence a block shows for its issue.
pub fn plugin_issue_text(translate: &YewI18n, kind: PluginTileKind, issue: &PluginRefIssue) -> String {
    match issue {
        PluginRefIssue::NotChosen => translate.t(ISSUE_NOT_CHOSEN),
        PluginRefIssue::Missing(id) => translate.t(ISSUE_MISSING).replace("{plugin}", id),
        PluginRefIssue::Unavailable(plugin) => translate
            .t(ISSUE_UNAVAILABLE)
            .replace("{plugin}", &plugin.name)
            .replace("{reason}", &unavailable_reason(translate, plugin)),
        PluginRefIssue::NotProvider(plugin) => {
            let key = match kind {
                PluginTileKind::Input => ISSUE_NOT_INPUT,
                PluginTileKind::Output => ISSUE_NOT_OUTPUT,
            };
            translate.t(key).replace("{plugin}", &plugin.name)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::{PluginLocaleMap, PluginUiService};
    use shared::model::{ConfigInputDto, InputType, PluginTargetOutputDto};
    use std::collections::HashMap;
    use yew::Callback;

    fn tile(title: &str) -> PluginSourceTileDto {
        PluginSourceTileDto {
            title: title.to_string(),
            title_key: None,
            icon: None,
            description: None,
            description_key: None,
        }
    }

    fn plugin(id: &str, state: PluginStateDto, input: Option<PluginSourceTileDto>) -> Rc<PluginUiDto> {
        Rc::new(PluginUiDto {
            id: id.to_string(),
            name: format!("{id} name"),
            version: "1.0.0".to_string(),
            state,
            failure: None,
            locales: Vec::new(),
            config_section: false,
            config_extensions: Vec::new(),
            config_tab: None,
            input_form: Vec::new(),
            output_form: Vec::new(),
            input_tile: input,
            output_tile: None,
            contributions: Vec::new(),
            rpc_exports: Vec::new(),
        })
    }

    fn context(plugins: Vec<Rc<PluginUiDto>>, strings: &[(&str, &[(&str, &str)])], loaded: bool) -> PluginUiContext {
        let strings: HashMap<String, Rc<PluginLocaleMap>> = strings
            .iter()
            .map(|(id, entries)| {
                let map: PluginLocaleMap = entries.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect();
                ((*id).to_string(), Rc::new(map))
            })
            .collect();
        PluginUiContext::for_tests(
            plugins,
            strings,
            Rc::new(PluginUiService::with_base("/".to_string())),
            Callback::noop(),
            loaded,
        )
    }

    #[test]
    fn only_enabled_providers_get_a_tile_sorted_by_title() {
        let mut described = tile("Zattoo");
        described.description = Some("TV".to_string());
        described.icon = Some("Live".to_string());
        let ui = context(
            vec![
                plugin("zattoo-input", PluginStateDto::Enabled, Some(described)),
                plugin("youtube-input", PluginStateDto::Enabled, Some(tile("YouTube"))),
                plugin("off-input", PluginStateDto::Disabled, Some(tile("Off"))),
                plugin("broken-input", PluginStateDto::Failed, Some(tile("Broken"))),
                plugin("notifier", PluginStateDto::Enabled, None),
            ],
            &[],
            true,
        );
        let bricks = plugin_bricks(&ui, PluginTileKind::Input);
        assert_eq!(bricks.iter().map(|b| b.plugin_id.as_str()).collect::<Vec<_>>(), ["youtube-input", "zattoo-input"]);
        assert_eq!(bricks[1].description.as_deref(), Some("TV"));
        assert_eq!(bricks[1].icon.as_deref(), Some("Live"));
        assert_eq!(plugin_bricks(&ui, PluginTileKind::Output), Vec::<PluginBrick>::new());
    }

    #[test]
    fn tile_texts_resolve_in_the_plugin_strings() {
        let mut keyed = tile("YouTube");
        keyed.title_key = Some("tile.title".to_string());
        keyed.description = Some("fallback".to_string());
        keyed.description_key = Some("tile.hint".to_string());
        let ui = context(
            vec![plugin("youtube-input", PluginStateDto::Enabled, Some(keyed))],
            &[("youtube-input", &[("tile.title", "YouTube-Kanäle"), ("tile.hint", "Kanäle und Playlists")])],
            true,
        );
        let brick = &plugin_bricks(&ui, PluginTileKind::Input)[0];
        assert_eq!(brick.title, "YouTube-Kanäle");
        assert_eq!(brick.description.as_deref(), Some("Kanäle und Playlists"));
        assert_eq!(
            plugin_block_label(Some(&ui), PluginTileKind::Input, "youtube-input").as_deref(),
            Some("YouTube-Kanäle")
        );
    }

    #[test]
    fn a_block_label_falls_back_to_the_plugin_name_then_the_id() {
        let ui = context(vec![plugin("youtube-input", PluginStateDto::Enabled, Some(tile("YouTube")))], &[], true);
        assert_eq!(plugin_block_label(Some(&ui), PluginTileKind::Input, "youtube-input").as_deref(), Some("YouTube"));
        assert_eq!(
            plugin_block_label(Some(&ui), PluginTileKind::Output, "youtube-input").as_deref(),
            Some("youtube-input name"),
            "no output tile: the plugin name"
        );
        assert_eq!(plugin_block_label(Some(&ui), PluginTileKind::Input, "gone").as_deref(), Some("gone"));
        assert_eq!(plugin_block_label(None, PluginTileKind::Input, "gone").as_deref(), Some("gone"));
        assert_eq!(plugin_block_label(Some(&ui), PluginTileKind::Input, " "), None);
    }

    #[test]
    fn issues_name_what_keeps_the_block_from_running() {
        let ui = context(
            vec![
                plugin("youtube-input", PluginStateDto::Enabled, Some(tile("YouTube"))),
                plugin("off-input", PluginStateDto::Disabled, Some(tile("Off"))),
                plugin("notifier", PluginStateDto::Enabled, None),
            ],
            &[],
            true,
        );
        let issue = |id: &str| plugin_ref_issue(Some(&ui), PluginTileKind::Input, id);
        assert_eq!(issue("youtube-input"), None);
        assert_eq!(issue(""), Some(PluginRefIssue::NotChosen));
        assert_eq!(issue("gone"), Some(PluginRefIssue::Missing("gone".to_string())));
        assert!(matches!(issue("off-input"), Some(PluginRefIssue::Unavailable(p)) if p.id == "off-input"));
        assert!(matches!(issue("notifier"), Some(PluginRefIssue::NotProvider(p)) if p.id == "notifier"));
    }

    #[test]
    fn nothing_but_a_missing_choice_is_claimed_before_the_list_is_loaded() {
        let unloaded = context(Vec::new(), &[], false);
        assert_eq!(plugin_ref_issue(Some(&unloaded), PluginTileKind::Input, "youtube-input"), None);
        assert_eq!(plugin_ref_issue(None, PluginTileKind::Output, "csv-out"), None);
        assert_eq!(plugin_ref_issue(None, PluginTileKind::Output, ""), Some(PluginRefIssue::NotChosen));
    }

    #[test]
    fn only_plugin_blocks_name_a_plugin() {
        let mut input = ConfigInputDto::new_with_type(InputType::Plugin);
        input.plugin = Some("youtube-input".into());
        let block = |block_type, instance| Block { id: 1, block_type, position: (0.0, 0.0), instance };
        assert_eq!(
            block_plugin_id(&block(BlockType::InputPlugin, BlockInstance::Input(Rc::new(input.clone())))),
            Some("youtube-input")
        );
        assert_eq!(
            block_plugin_id(&block(
                BlockType::InputPlugin,
                BlockInstance::Input(Rc::new(ConfigInputDto::new_with_type(InputType::Plugin)))
            )),
            Some("")
        );
        assert_eq!(block_plugin_id(&block(BlockType::InputM3u, BlockInstance::Input(Rc::new(input)))), None);
        let output =
            TargetOutputDto::Plugin(PluginTargetOutputDto { plugin: "csv-out".to_string(), ..Default::default() });
        assert_eq!(
            block_plugin_id(&block(BlockType::OutputPlugin, BlockInstance::Output(Rc::new(output)))),
            Some("csv-out")
        );
    }

    #[test]
    fn the_keys_exist_in_every_locale() {
        for locale in [
            include_str!("../../../../public/assets/i18n/en.json"),
            include_str!("../../../../public/assets/i18n/es.json"),
            include_str!("../../../../public/assets/i18n/ar.json"),
            include_str!("../../../../public/assets/i18n/ru.json"),
        ] {
            let translations: serde_json::Value = serde_json::from_str(locale).unwrap();
            for key in PLUGIN_TILE_KEYS {
                let pointer = format!("/{}", key.replace('.', "/"));
                assert!(translations.pointer(&pointer).and_then(serde_json::Value::as_str).is_some(), "{pointer}");
            }
        }
    }
}
