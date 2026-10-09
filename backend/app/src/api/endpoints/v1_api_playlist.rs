// Recording-source resolution moved into the recording subsystem so it no
// longer has to call back into `endpoints`.
use crate::api::{endpoints::extract_accept_header::ExtractAcceptHeader, model::AppState};
use axum::response::IntoResponse;
use shared::model::{PlaylistRequest, XtreamCluster};
use std::sync::Arc;

const PLAYLIST_UPDATE_STATUS_READ_CONCURRENCY: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::api) struct ResolvedRecordingSource {
    pub virtual_id: u32,
    pub input_name: String,
    pub title: String,
    /// Playlist group the item belongs to; blank groups are `None`.
    pub group: Option<String>,
    /// Series name shared by all episodes. Episodes carry it as their
    /// `name`, while `title` is the episode's own title.
    pub series_name: Option<String>,
    pub extension: Option<String>,
    pub downloadable: bool,
}

macro_rules! create_player_api_for_cluster {
    ($fn_name:ident, $cluster:expr) => {
        async fn $fn_name(
            ExtractAcceptHeader(accept): ExtractAcceptHeader,
            axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
            axum::extract::Json(playlist_req): axum::extract::Json<PlaylistRequest>,
        ) -> impl IntoResponse + Send {
            playlist_content(accept.clone(), &app_state, &playlist_req, $cluster).await.into_response()
        }
    };
}

create_player_api_for_cluster!(playlist_content_live, XtreamCluster::Live);

create_player_api_for_cluster!(playlist_content_vod, XtreamCluster::Video);

create_player_api_for_cluster!(playlist_content_series, XtreamCluster::Series);

const FILTER_PREVIEW_DEFAULT_SAMPLES: u16 = 25;

const FILTER_PREVIEW_MAX_SAMPLES: u16 = 50;

#[cfg(test)]
mod epg_channel_candidate_tests;

#[cfg(test)]
mod tests;

mod content;
mod epg;
mod error;
mod filter_preview;
mod recording;
mod resources;
mod routes;
mod update;
mod webplayer;

#[cfg(test)]
use self::epg::load_epg_channels_for_input;
#[cfg(test)]
use self::epg::merge_epg_channels;
#[cfg(test)]
use self::recording::epg_channel_id_matches;
#[cfg(test)]
use self::recording::select_epg_channel_candidate;
#[cfg(test)]
use self::recording::RecordingStreamQuery;
pub(in crate::api) use self::recording::{
    build_webplayer_recording_url, resolve_target_live_recording_source_by_epg_channel, resolve_target_recording_source,
};
#[cfg(test)]
use self::recording::{recording_candidate, RecordingCandidateFields};
#[cfg(test)]
use self::resources::resolve_provider_url_for_request;
pub use self::routes::{
    v1_api_playlist_register_protected, v1_api_playlist_register_public, v1_api_playlist_register_with_permissions,
};
#[cfg(test)]
use self::update::read_playlist_update_input_status;
#[cfg(test)]
use self::update::resolve_manual_playlist_update_targets;
use self::{
    content::{playlist_content, playlist_episode_item, playlist_series_info},
    epg::playlist_epg,
    error::ManualUpdateEnqueueError,
    filter_preview::playlist_filter_preview,
    recording::playlist_recording_stream,
    resources::{
        create_config_input_for_m3u, create_config_input_for_xtream, playlist_resolve_url, playlist_resource,
        resolve_provider_url_with_input,
    },
    update::{playlist_update, playlist_update_status},
    webplayer::{build_playlist_webplayer_url, playlist_webplayer, playlist_webplayer_stream},
};
