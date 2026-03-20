use shared::error::{info_err_res, TuliproxError};
use std::{fmt, str::FromStr};

const DASHBOARD: &str = "dashboard";
const STATS: &str = "stats";
const STREAMS: &str = "streams";
const USERS: &str = "users";
const CONFIG: &str = "config";
const PLAYLIST_UPDATE: &str = "playlist_update";
const PLAYLIST_SETTINGS: &str = "PlaylistSettings";
const PLAYLIST_SETTINGS_LOWER: &str = "playlistsettings";
const PLAYLIST_SETTINGS_SNAKE: &str = "playlist_settings";
const PLAYLIST_EDITOR_LEGACY: &str = "playlist_editor";
const PLAYLIST_EXPLORER: &str = "playlist_explorer";
const PLAYLIST_EPG: &str = "playlist_epg";
const RBAC: &str = "rbac";
const SOURCE_EDITOR: &str = "source_editor";

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ViewType {
    Dashboard,
    Stats,
    Streams,
    Users,
    Config,
    SourceEditor,
    PlaylistUpdate,
    PlaylistSettings,
    PlaylistExplorer,
    PlaylistEpg,
    Rbac,
}

impl FromStr for ViewType {
    type Err = TuliproxError;

    fn from_str(s: &str) -> Result<Self, TuliproxError> {
        match s.to_lowercase().as_str() {
            DASHBOARD => Ok(ViewType::Dashboard),
            STATS => Ok(ViewType::Stats),
            STREAMS => Ok(ViewType::Streams),
            USERS => Ok(ViewType::Users),
            CONFIG => Ok(ViewType::Config),
            SOURCE_EDITOR => Ok(ViewType::SourceEditor),
            PLAYLIST_UPDATE => Ok(ViewType::PlaylistUpdate),
            PLAYLIST_SETTINGS_LOWER | PLAYLIST_SETTINGS_SNAKE | PLAYLIST_EDITOR_LEGACY => {
                Ok(ViewType::PlaylistSettings)
            }
            PLAYLIST_EXPLORER => Ok(ViewType::PlaylistExplorer),
            PLAYLIST_EPG => Ok(ViewType::PlaylistEpg),
            RBAC => Ok(ViewType::Rbac),
            _ => info_err_res!("Unknown view type: {s}"),
        }
    }
}

impl fmt::Display for ViewType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ViewType::Dashboard => DASHBOARD,
            ViewType::Stats => STATS,
            ViewType::Streams => STREAMS,
            ViewType::Users => USERS,
            ViewType::Config => CONFIG,
            ViewType::SourceEditor => SOURCE_EDITOR,
            ViewType::PlaylistUpdate => PLAYLIST_UPDATE,
            ViewType::PlaylistSettings => PLAYLIST_SETTINGS,
            ViewType::PlaylistExplorer => PLAYLIST_EXPLORER,
            ViewType::PlaylistEpg => PLAYLIST_EPG,
            ViewType::Rbac => RBAC,
        };
        write!(f, "{s}")
    }
}
