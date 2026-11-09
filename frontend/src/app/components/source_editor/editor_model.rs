use serde::{Deserialize, Serialize};
use shared::model::{ConfigInputDto, ConfigTargetDto, InputType, TargetOutputDto};
use std::{fmt, rc::Rc};
use yew::{Callback, UseStateHandle};

pub const BLOCK_WIDTH: f32 = 100.0;
pub const BLOCK_HEIGHT: f32 = 50.0;
pub const BLOCK_HEADER_HEIGHT: f32 = 12.0;
pub const BLOCK_PORT_HEIGHT: f32 = 10.0;

// ----------------- Data Models -----------------
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub enum BlockType {
    InputXtream,
    InputM3u,
    InputLibrary,
    InputEmby,
    InputJellyfin,
    InputPlex,
    InputStalker,
    InputStaged,
    /// A `type: plugin` input: the id of the providing plugin lives in
    /// `ConfigInputDto::plugin` and is picked in the source editor.
    InputPlugin,
    Target,
    OutputM3u,
    OutputXtream,
    OutputHdHomeRun,
    OutputStrm,
    /// A `type: plugin` output: `TargetOutputDto::Plugin`, bound to an installed
    /// plugin that renders it.
    OutputPlugin,
}

// Define string constants
impl BlockType {
    pub const INPUT_XTREAM: &'static str = "InputXtream";
    pub const INPUT_M3U: &'static str = "InputM3u";
    pub const INPUT_LIBRARY: &'static str = "InputLibrary";
    pub const INPUT_EMBY: &'static str = "InputEmby";
    pub const INPUT_JELLYFIN: &'static str = "InputJellyfin";
    pub const INPUT_PLEX: &'static str = "InputPlex";
    pub const INPUT_STALKER: &'static str = "InputStalker";
    pub const INPUT_STAGED: &'static str = "InputStaged";
    pub const INPUT_PLUGIN: &'static str = "InputPlugin";
    pub const TARGET: &'static str = "Target";
    pub const OUTPUT_M3U: &'static str = "OutputM3u";
    pub const OUTPUT_XTREAM: &'static str = "OutputXtream";
    pub const OUTPUT_HDHOMERUN: &'static str = "OutputHdHomeRun";
    pub const OUTPUT_STRM: &'static str = "OutputStrm";
    pub const OUTPUT_PLUGIN: &'static str = "OutputPlugin";

    pub fn is_input(&self) -> bool {
        matches!(
            self,
            Self::InputXtream
                | Self::InputM3u
                | Self::InputLibrary
                | Self::InputEmby
                | Self::InputJellyfin
                | Self::InputPlex
                | Self::InputStalker
                | Self::InputStaged
                | Self::InputPlugin
        )
    }

    pub fn is_target(&self) -> bool { matches!(self, Self::Target) }

    pub fn is_output(&self) -> bool {
        matches!(
            self,
            Self::OutputXtream | Self::OutputM3u | Self::OutputHdHomeRun | Self::OutputStrm | Self::OutputPlugin
        )
    }

    pub fn is_chainable_input(&self) -> bool { matches!(self, Self::InputM3u | Self::InputXtream) }

    /// A `type: plugin` input or output: the block names the plugin it runs on.
    pub fn is_plugin(&self) -> bool { matches!(self, Self::InputPlugin | Self::OutputPlugin) }

    /// Left port — block can receive a connection.
    pub fn has_input_port(&self) -> bool { self.is_target() || self.is_output() || self.is_chainable_input() }

    /// Right port — block can be the source of a connection.
    pub fn has_output_port(&self) -> bool { self.is_target() || self.is_input() }
}

// Convert from String to BlockType
impl From<&str> for BlockType {
    fn from(s: &str) -> Self {
        match s {
            BlockType::INPUT_XTREAM => BlockType::InputXtream,
            BlockType::INPUT_M3U => BlockType::InputM3u,
            BlockType::INPUT_LIBRARY => BlockType::InputLibrary,
            BlockType::INPUT_EMBY => BlockType::InputEmby,
            BlockType::INPUT_JELLYFIN => BlockType::InputJellyfin,
            BlockType::INPUT_PLEX => BlockType::InputPlex,
            BlockType::INPUT_STALKER => BlockType::InputStalker,
            BlockType::INPUT_STAGED => BlockType::InputStaged,
            BlockType::INPUT_PLUGIN => BlockType::InputPlugin,
            BlockType::TARGET => BlockType::Target,
            BlockType::OUTPUT_M3U => BlockType::OutputM3u,
            BlockType::OUTPUT_XTREAM => BlockType::OutputXtream,
            BlockType::OUTPUT_HDHOMERUN => BlockType::OutputHdHomeRun,
            BlockType::OUTPUT_STRM => BlockType::OutputStrm,
            BlockType::OUTPUT_PLUGIN => BlockType::OutputPlugin,
            _ => BlockType::Target, // fallback
        }
    }
}

impl From<String> for BlockType {
    fn from(s: String) -> Self { BlockType::from(s.as_str()) }
}

impl From<InputType> for BlockType {
    fn from(s: InputType) -> Self {
        match s {
            InputType::M3uBatch | InputType::M3u => BlockType::InputM3u,
            InputType::XtreamBatch | InputType::Xtream => BlockType::InputXtream,
            InputType::Library => BlockType::InputLibrary,
            InputType::Emby => BlockType::InputEmby,
            InputType::Jellyfin => BlockType::InputJellyfin,
            InputType::Plex => BlockType::InputPlex,
            InputType::Stalker | InputType::StalkerBatch => BlockType::InputStalker,
            InputType::Staged => BlockType::InputStaged,
            InputType::Plugin => BlockType::InputPlugin,
        }
    }
}

// Display trait using constants
impl fmt::Display for BlockType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            BlockType::InputXtream => Self::INPUT_XTREAM,
            BlockType::InputM3u => Self::INPUT_M3U,
            BlockType::InputLibrary => Self::INPUT_LIBRARY,
            BlockType::InputEmby => Self::INPUT_EMBY,
            BlockType::InputJellyfin => Self::INPUT_JELLYFIN,
            BlockType::InputPlex => Self::INPUT_PLEX,
            BlockType::InputStalker => Self::INPUT_STALKER,
            BlockType::InputStaged => Self::INPUT_STAGED,
            BlockType::InputPlugin => Self::INPUT_PLUGIN,
            BlockType::Target => Self::TARGET,
            BlockType::OutputM3u => Self::OUTPUT_M3U,
            BlockType::OutputXtream => Self::OUTPUT_XTREAM,
            BlockType::OutputHdHomeRun => Self::OUTPUT_HDHOMERUN,
            BlockType::OutputStrm => Self::OUTPUT_STRM,
            BlockType::OutputPlugin => Self::OUTPUT_PLUGIN,
        };
        write!(f, "{s}")
    }
}
pub(crate) type BlockId = u16;

/// What a sidebar tile adds: a block type and, for a plugin tile, the plugin
/// the new input or output runs on.
#[derive(Clone, Debug, PartialEq)]
pub struct BlockSpec {
    pub block_type: BlockType,
    /// Only for [`BlockType::is_plugin`] types; `None` adds a plugin block with
    /// no plugin chosen yet.
    pub plugin: Option<String>,
}

impl BlockSpec {
    pub const fn of(block_type: BlockType) -> Self { Self { block_type, plugin: None } }

    pub fn with_plugin(block_type: BlockType, plugin_id: &str) -> Self {
        Self { block_type, plugin: block_type.is_plugin().then(|| plugin_id.to_string()) }
    }

    /// The `text/plain` payload of a sidebar drag: `InputXtream`, or
    /// `InputPlugin:youtube-input` for a plugin tile. A plugin id never holds a
    /// `:`, so the first one splits.
    pub fn to_drag_data(&self) -> String {
        match &self.plugin {
            Some(plugin_id) => format!("{}:{plugin_id}", self.block_type),
            None => self.block_type.to_string(),
        }
    }

    /// The inverse of [`Self::to_drag_data`]. A plugin id on a block type that
    /// takes none, or a blank one, is dropped.
    pub fn from_drag_data(data: &str) -> Self {
        let (block_type, plugin_id) = data.split_once(':').unwrap_or((data, ""));
        let block_type = BlockType::from(block_type.trim());
        let plugin_id = plugin_id.trim();
        if plugin_id.is_empty() {
            Self::of(block_type)
        } else {
            Self::with_plugin(block_type, plugin_id)
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Block {
    pub id: BlockId,
    pub block_type: BlockType,
    pub position: (f32, f32),
    pub instance: BlockInstance,
}

impl Block {
    /// Liefert die Bounding-Box des Blocks unter Berücksichtigung der Canvas-Verschiebung.
    pub fn bounds(&self, canvas_offset: (f32, f32), zoom_factor: f32) -> (f32, f32, f32, f32) {
        let (ox, oy) = canvas_offset;
        let bx = (self.position.0 * zoom_factor) + ox;
        let by = (self.position.1 * zoom_factor) + oy;
        let b_left = bx;
        let b_top = by;
        let b_right = bx + (BLOCK_WIDTH * zoom_factor);
        let b_bottom = by + ((BLOCK_HEIGHT + BLOCK_HEADER_HEIGHT + BLOCK_PORT_HEIGHT) * zoom_factor);
        (b_left, b_top, b_right, b_bottom)
    }

    /// Prüft, ob der Block von einem Auswahlrechteck getroffen wird.
    pub fn intersects_rect(
        &self,
        rect_start: (f32, f32),
        rect_end: (f32, f32),
        canvas_offset: (f32, f32),
        zoom_factor: f32,
    ) -> bool {
        let (b_left, b_top, b_right, b_bottom) = self.bounds(canvas_offset, zoom_factor);

        // Rechteck normalisieren
        let (x1, y1) = rect_start;
        let (x2, y2) = rect_end;
        let r_left = x1.min(x2);
        let r_top = y1.min(y2);
        let r_right = x1.max(x2);
        let r_bottom = y1.max(y2);

        b_right >= r_left && b_left <= r_right && b_bottom >= r_top && b_top <= r_bottom
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Connection {
    pub from: BlockId,
    pub to: BlockId,
}

#[derive(Clone, Copy, PartialEq)]
pub enum PortStatus {
    Valid,
    Invalid,
    Inactive,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BlockInstance {
    Input(Rc<ConfigInputDto>),
    Target(Rc<ConfigTargetDto>),
    Output(Rc<TargetOutputDto>),
}

#[derive(Clone, PartialEq)]
pub enum EditMode {
    Inactive,
    Active(Block),
}

#[derive(Clone)]
pub struct SourceEditorContext {
    pub on_form_change: Callback<(BlockId, BlockInstance)>,
    pub open_target_bouquet: Callback<String>,
    pub bouquet_revision: u64,
    pub output_curation_managed: bool,
    pub edit_mode: UseStateHandle<EditMode>,
    pub allow_write: bool,
}

impl PartialEq for SourceEditorContext {
    fn eq(&self, _other: &Self) -> bool { false }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_specs_round_trip_through_the_drag_payload() {
        let plain = BlockSpec::of(BlockType::InputXtream);
        assert_eq!(plain.to_drag_data(), "InputXtream");
        assert_eq!(BlockSpec::from_drag_data("InputXtream"), plain);

        let input = BlockSpec::with_plugin(BlockType::InputPlugin, "youtube-input");
        assert_eq!(input.to_drag_data(), "InputPlugin:youtube-input");
        assert_eq!(BlockSpec::from_drag_data("InputPlugin:youtube-input"), input);

        let output = BlockSpec::with_plugin(BlockType::OutputPlugin, "csv-out");
        assert_eq!(BlockSpec::from_drag_data(&output.to_drag_data()), output);
    }

    #[test]
    fn a_plugin_id_only_sticks_to_plugin_blocks() {
        assert_eq!(BlockSpec::with_plugin(BlockType::InputM3u, "x").plugin, None);
        assert_eq!(BlockSpec::from_drag_data("InputM3u:x"), BlockSpec::of(BlockType::InputM3u));
        assert_eq!(BlockSpec::from_drag_data("InputPlugin:  "), BlockSpec::of(BlockType::InputPlugin));
        assert_eq!(BlockSpec::from_drag_data("InputPlugin"), BlockSpec::of(BlockType::InputPlugin));
        assert!(BlockType::InputPlugin.is_plugin() && BlockType::OutputPlugin.is_plugin());
        assert!(!BlockType::InputXtream.is_plugin() && !BlockType::Target.is_plugin());
    }

    #[test]
    fn media_server_input_types_keep_distinct_source_editor_blocks() {
        assert_eq!(BlockType::from(InputType::Emby), BlockType::InputEmby);
        assert_eq!(BlockType::from(InputType::Jellyfin), BlockType::InputJellyfin);
        assert_eq!(BlockType::from(InputType::Plex), BlockType::InputPlex);
    }

    #[test]
    fn media_server_source_editor_blocks_are_inputs() {
        assert!(BlockType::InputEmby.is_input());
        assert!(BlockType::InputJellyfin.is_input());
        assert!(BlockType::InputPlex.is_input());
    }

    #[test]
    fn stalker_input_types_share_source_editor_block() {
        assert_eq!(BlockType::from(InputType::Stalker), BlockType::InputStalker);
        assert_eq!(BlockType::from(InputType::StalkerBatch), BlockType::InputStalker);
        assert!(BlockType::InputStalker.is_input());
    }
}
