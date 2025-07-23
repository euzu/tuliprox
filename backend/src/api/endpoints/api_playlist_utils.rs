use crate::model::{AppConfig, Config, ConfigInput, ConfigTarget};
use crate::repository::{m3u_repository, xtream_repository};
use crate::utils::{m3u, xtream};
use crate::utils;
use axum::response::IntoResponse;
use indexmap::IndexMap;
use serde::Serialize;
use serde_json::{json, Value};
use shared::model::{InputType, M3uPlaylistItem, PlaylistGroup, PlaylistItemType, TargetType, XtreamCluster};
use std::sync::Arc;

#[derive(serde::Serialize, serde::Deserialize)]
struct PlaylistResponseGroup {
    id: u32,
    title: String,
    channels: serde_json::Value,
    xtream_cluster: XtreamCluster,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct PlaylistResponse {
    live: Option<Vec<PlaylistResponseGroup>>,
    vod: Option<Vec<PlaylistResponseGroup>>,
    series: Option<Vec<PlaylistResponseGroup>>,
}

fn group_playlist_items<T>(
    cluster: XtreamCluster,
    iter: impl Iterator<Item=T>,
    get_group: fn(&T) -> String,
) -> Vec<PlaylistResponseGroup>
where
    T: Serialize,
{
    let mut groups: IndexMap<String, Vec<T>> = IndexMap::new();

    for item in iter {
        let group_key = get_group(&item);
        groups.entry(group_key)
            .or_default()
            .push(item);
    }

    groups
        .into_iter()
        .enumerate()
        .map(|(index, (key, value))| PlaylistResponseGroup {
            #[allow(clippy::cast_possible_truncation)]
            id: index as u32,
            title: key.to_string(),
            channels: serde_json::to_value(value).unwrap_or(Value::Null),
            xtream_cluster: cluster,
        })
        .collect()
}

fn group_playlist_items_by_cluster(params: Option<(utils::FileReadGuard,
                                                   impl Iterator<Item=(M3uPlaylistItem, bool)>)>) ->
                                   (Vec<M3uPlaylistItem>, Vec<M3uPlaylistItem>, Vec<M3uPlaylistItem>) {
    match params {
        None => (vec![], vec![], vec![]),
        Some((guard, iter)) => {
            let mut live = Vec::new();
            let mut video = Vec::new();
            let mut series = Vec::new();
            for (item, _) in iter {
                match item.item_type {
                    PlaylistItemType::Live
                    | PlaylistItemType::LiveUnknown
                    | PlaylistItemType::LiveHls
                    | PlaylistItemType::LiveDash => {
                        live.push(item);
                    }
                    PlaylistItemType::Catchup
                    | PlaylistItemType::Video => {
                        video.push(item);
                    }
                    PlaylistItemType::Series
                    | PlaylistItemType::SeriesInfo => {
                        series.push(item);
                    }
                }
            }

            drop(guard);

            (live, video, series)
        }
    }
}

fn group_playlist_groups_by_cluster(playlist: Vec<PlaylistGroup>, input_type: InputType) -> (Vec<PlaylistResponseGroup>, Vec<PlaylistResponseGroup>, Vec<PlaylistResponseGroup>) {
    let mut live = Vec::new();
    let mut video = Vec::new();
    let mut series = Vec::new();
    for group in playlist {
        let channels = group.channels.iter()
            .filter_map(|item| {
                if input_type == InputType::M3u {
                    serde_json::to_value(item.to_m3u())
                } else {
                    serde_json::to_value(item.to_xtream())
                }.ok()
            })
            .collect::<serde_json::Value>();
        let grp = PlaylistResponseGroup {
            id: group.id,
            title: group.title,
            channels,
            xtream_cluster: group.xtream_cluster,
        };
        match group.xtream_cluster {
            XtreamCluster::Live => live.push(grp),
            XtreamCluster::Video => video.push(grp),
            XtreamCluster::Series => series.push(grp),
        }
    }
    (live, video, series)
}


// async fn get_categories_content(action: Result<(Option<PathBuf>, Option<String>), std::io::Error>) -> Option<String> {
//     if let Ok((Some(file_path), _content)) = action {
//         if let Ok(content) = tokio::fs::read_to_string(&file_path).await {
//             // TODO deserialize like sax parser
//             if let Ok(categories) = serde_json::from_str::<Vec<PlaylistXtreamCategory>>(&content) {
//                 return serde_json::to_string(&categories).ok();
//             }
//         }
//     }
//     None
// }


async fn grouped_channels(
    cfg: &AppConfig,
    target: &ConfigTarget,
    cluster: XtreamCluster,
) -> Option<Vec<PlaylistResponseGroup>> {
    xtream_repository::iter_raw_xtream_playlist(cfg, target, cluster).await
        .map(|(_guard, iter)| group_playlist_items(
            cluster,
            iter.map(|(v, _)| v),
            |item| item.group.clone(),
        ))
}

pub(in crate::api::endpoints) async fn get_playlist_for_target(cfg_target: Option<&ConfigTarget>, cfg: &AppConfig) -> impl axum::response::IntoResponse + Send {
    if let Some(target) = cfg_target {
        if target.has_output(&TargetType::Xtream) {
            let live_channels = grouped_channels(cfg, target, XtreamCluster::Live).await;
            let vod_channels = grouped_channels(cfg, target, XtreamCluster::Video).await;
            let series_channels = grouped_channels(cfg, target, XtreamCluster::Series).await;

            let response = PlaylistResponse {
                live: live_channels,
                vod: vod_channels,
                series: series_channels,
            };

            return (axum::http::StatusCode::OK, axum::Json(response)).into_response();
        } else if target.has_output(&TargetType::M3u) {
            let all_channels = m3u_repository::iter_raw_m3u_playlist(cfg, target).await;
            let (live_channels, vod_channels, series_channels) = group_playlist_items_by_cluster(all_channels);
            let response = PlaylistResponse {
                live: Some(group_playlist_items(XtreamCluster::Live, live_channels.into_iter(), |item| item.group.clone())),
                vod: Some(group_playlist_items(XtreamCluster::Video, vod_channels.into_iter(), |item| item.group.clone())),
                series: Some(group_playlist_items(XtreamCluster::Series, series_channels.into_iter(), |item| item.group.clone())),
            };

            return (axum::http::StatusCode::OK, axum::Json(response)).into_response();
        }
    }
    (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": "Invalid Arguments"}))).into_response()
}

pub(in crate::api::endpoints) async fn get_playlist(client: Arc<reqwest::Client>, cfg_input: Option<&ConfigInput>, cfg: &Config) -> impl IntoResponse + Send {
    match cfg_input {
        Some(input) => {
            let (result, errors) =
                match input.input_type {
                    InputType::M3u | InputType::M3uBatch => m3u::get_m3u_playlist(client, cfg, input, &cfg.working_dir).await,
                    InputType::Xtream | InputType::XtreamBatch => xtream::get_xtream_playlist(cfg, client, input, &cfg.working_dir).await,
                };
            if result.is_empty() {
                let error_strings: Vec<String> = errors.iter().map(std::string::ToString::to_string).collect();
                (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": error_strings.join(", ")}))).into_response()
            } else {
                let (live, vod, series) = group_playlist_groups_by_cluster(result, input.input_type);
                let response = PlaylistResponse {
                    live: Some(live),
                    vod: Some(vod),
                    series: Some(series),
                };
                (axum::http::StatusCode::OK, axum::Json(response)).into_response()
            }
        }
        None => (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": "Invalid Arguments"}))).into_response(),
    }
}
