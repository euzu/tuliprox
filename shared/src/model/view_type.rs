use crate::{error::TuliproxError, utils::Internable};
use serde::{Deserialize, Deserializer, Serialize};
use std::{fmt, str::FromStr, sync::Arc};
use strum_macros::EnumIter;

const DASHBOARD: &str = "dashboard";
const STATS: &str = "stats";
const STREAMS: &str = "streams";
const DOWNLOADS: &str = "downloads";
const USERS: &str = "users";
const CONFIG: &str = "config";
const PLAYLIST_UPDATE: &str = "playlist_update";
const PLAYLIST_SETTINGS: &str = "playlist_settings";
const PLAYLIST_EXPLORER: &str = "playlist_explorer";
const PLAYLIST_EPG: &str = "playlist_epg";
const RBAC: &str = "rbac";
const SOURCE_EDITOR: &str = "source_editor";
const STREAM_HISTORY: &str = "stream_history";
const RECORDING_LIBRARY: &str = "recording_library";
const RECORDING_RULES: &str = "recording_rules";
const RECORDING_RULE_FORM: &str = "recording_rule_form";

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default, EnumIter)]
pub enum ViewType {
    #[default]
    Dashboard,
    Stats,
    Streams,
    StreamHistory,
    Downloads,
    Users,
    Config,
    SourceEditor,
    PlaylistUpdate,
    PlaylistSettings,
    PlaylistExplorer,
    PlaylistEpg,
    Rbac,
    RecordingLibrary,
    RecordingRules,
    RecordingRuleForm,
}

impl ViewType {
    pub fn is_default(&self) -> bool { matches!(self, ViewType::Dashboard) }
    pub fn as_str(&self) -> &'static str {
        match self {
            ViewType::Dashboard => DASHBOARD,
            ViewType::Stats => STATS,
            ViewType::Streams => STREAMS,
            ViewType::StreamHistory => STREAM_HISTORY,
            ViewType::Downloads => DOWNLOADS,
            ViewType::Users => USERS,
            ViewType::Config => CONFIG,
            ViewType::SourceEditor => SOURCE_EDITOR,
            ViewType::PlaylistUpdate => PLAYLIST_UPDATE,
            ViewType::PlaylistSettings => PLAYLIST_SETTINGS,
            ViewType::PlaylistExplorer => PLAYLIST_EXPLORER,
            ViewType::PlaylistEpg => PLAYLIST_EPG,
            ViewType::Rbac => RBAC,
            ViewType::RecordingLibrary => RECORDING_LIBRARY,
            ViewType::RecordingRules => RECORDING_RULES,
            ViewType::RecordingRuleForm => RECORDING_RULE_FORM,
        }
    }
}

impl FromStr for ViewType {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, TuliproxError> {
        match s.to_lowercase().as_str() {
            DASHBOARD => Ok(ViewType::Dashboard),
            STATS => Ok(ViewType::Stats),
            STREAMS => Ok(ViewType::Streams),
            STREAM_HISTORY => Ok(ViewType::StreamHistory),
            DOWNLOADS => Ok(ViewType::Downloads),
            USERS => Ok(ViewType::Users),
            CONFIG => Ok(ViewType::Config),
            SOURCE_EDITOR => Ok(ViewType::SourceEditor),
            PLAYLIST_UPDATE => Ok(ViewType::PlaylistUpdate),
            PLAYLIST_SETTINGS => Ok(ViewType::PlaylistSettings),
            PLAYLIST_EXPLORER => Ok(ViewType::PlaylistExplorer),
            PLAYLIST_EPG => Ok(ViewType::PlaylistEpg),
            RBAC => Ok(ViewType::Rbac),
            RECORDING_LIBRARY => Ok(ViewType::RecordingLibrary),
            RECORDING_RULES => Ok(ViewType::RecordingRules),
            RECORDING_RULE_FORM => Ok(ViewType::RecordingRuleForm),
            _ => Err(TuliproxError::Config(format!("Unknown view type: {s}"))),
        }
    }
}

impl fmt::Display for ViewType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = self.as_str();
        write!(f, "{s}")
    }
}

impl Internable for ViewType {
    fn intern(self) -> Arc<str> { self.as_str().intern() }
}

impl<'de> Deserialize<'de> for ViewType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        ViewType::from_str(&s).map_err(serde::de::Error::custom)
    }
}
impl Serialize for ViewType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}
