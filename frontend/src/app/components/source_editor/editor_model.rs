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
    Target,
    OutputM3u,
    OutputXtream,
    OutputHdHomeRun,
    OutputStrm,
}

// Define string constants
impl BlockType {
    pub const INPUT_XTREAM: &'static str = "InputXtream";
    pub const INPUT_M3U: &'static str = "InputM3u";
    pub const INPUT_LIBRARY: &'static str = "InputLibrary";
    pub const TARGET: &'static str = "Target";
    pub const OUTPUT_M3U: &'static str = "OutputM3u";
    pub const OUTPUT_XTREAM: &'static str = "OutputXtream";
    pub const OUTPUT_HDHOMERUN: &'static str = "OutputHdHomeRun";
    pub const OUTPUT_STRM: &'static str = "OutputStrm";

    pub fn is_input(&self) -> bool { matches!(self, Self::InputXtream | Self::InputM3u | Self::InputLibrary) }

    pub fn is_target(&self) -> bool { matches!(self, Self::Target) }

    pub fn is_output(&self) -> bool {
        matches!(self, Self::OutputXtream | Self::OutputM3u | Self::OutputHdHomeRun | Self::OutputStrm)
    }
}

// Convert from String to BlockType
impl From<&str> for BlockType {
    fn from(s: &str) -> Self {
        match s {
            BlockType::INPUT_XTREAM => BlockType::InputXtream,
            BlockType::INPUT_M3U => BlockType::InputM3u,
            BlockType::INPUT_LIBRARY => BlockType::InputLibrary,
            BlockType::TARGET => BlockType::Target,
            BlockType::OUTPUT_M3U => BlockType::OutputM3u,
            BlockType::OUTPUT_XTREAM => BlockType::OutputXtream,
            BlockType::OUTPUT_HDHOMERUN => BlockType::OutputHdHomeRun,
            BlockType::OUTPUT_STRM => BlockType::OutputStrm,
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
            BlockType::Target => Self::TARGET,
            BlockType::OutputM3u => Self::OUTPUT_M3U,
            BlockType::OutputXtream => Self::OUTPUT_XTREAM,
            BlockType::OutputHdHomeRun => Self::OUTPUT_HDHOMERUN,
            BlockType::OutputStrm => Self::OUTPUT_STRM,
        };
        write!(f, "{}", s)
    }
}
pub(crate) type BlockId = u16;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Block {
    pub id: BlockId,
    pub block_type: BlockType,
    pub position: (f32, f32),
    pub instance: BlockInstance,
}

impl Block {
    /// Liefert die Bounding-Box des Blocks unter Berücksichtigung der Canvas-Verschiebung.
    pub fn bounds(&self, canvas_offset: (f32, f32)) -> (f32, f32, f32, f32) {
        let (ox, oy) = canvas_offset;
        let bx = self.position.0 + ox;
        let by = self.position.1 + oy;
        let b_left = bx;
        let b_top = by;
        let b_right = bx + BLOCK_WIDTH;
        let b_bottom = by + (BLOCK_HEIGHT + BLOCK_HEADER_HEIGHT + BLOCK_PORT_HEIGHT);
        (b_left, b_top, b_right, b_bottom)
    }

    /// Prüft, ob der Block von einem Auswahlrechteck getroffen wird.
    pub fn intersects_rect(&self, rect_start: (f32, f32), rect_end: (f32, f32), canvas_offset: (f32, f32)) -> bool {
        let (b_left, b_top, b_right, b_bottom) = self.bounds(canvas_offset);

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
    pub edit_mode: UseStateHandle<EditMode>,
}

impl PartialEq for SourceEditorContext {
    fn eq(&self, _other: &Self) -> bool {
        false
    }
}
