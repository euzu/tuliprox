//! Web UI EPG grid: playlist groups and one group's channels with lean programmes.

use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Rows per grid response; the client shows a hint when a group has more channels.
pub const MAX_EPG_GRID_ROWS: usize = 1000;

/// Channel name filter of the EPG grid search. `Text` matches case-insensitively.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum EpgChannelFilter {
    Text(String),
    Regexp(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EpgGroupsRequest {
    pub target_id: u16,
    /// Only groups with matching channels; `channel_count` then counts the matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<EpgChannelFilter>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EpgGroupInfo {
    pub name: Arc<str>,
    pub channel_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EpgGridRequest {
    pub target_id: u16,
    pub group: String,
    /// Only matching channels; applied before `MAX_EPG_GRID_ROWS`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<EpgChannelFilter>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EpgGridProgrammeDto {
    pub start: i64,
    pub stop: i64,
    pub title: Option<Arc<str>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EpgGridRow {
    pub virtual_id: u32,
    pub name: Arc<str>,
    pub logo: Arc<str>,
    pub epg_channel_id: Arc<str>,
    pub programmes: Vec<EpgGridProgrammeDto>,
}
