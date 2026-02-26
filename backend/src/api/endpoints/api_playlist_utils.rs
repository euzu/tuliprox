use crate::{
    api::api_utils::{empty_json_list_response, json_or_bin_response, stream_json_or_bin_response_stream},
    model::{AppConfig, ConfigInput, ConfigTarget},
    repository::{
        iter_raw_m3u_input_playlist, iter_raw_m3u_target_playlist, iter_raw_xtream_input_playlist,
        iter_raw_xtream_target_playlist,
    },
    utils::{m3u, xtream},
};
use axum::response::IntoResponse;
use log::warn;
use serde_json::json;
use shared::{
    model::{
        InputType, M3uPlaylistItem, PlaylistItemType, TargetType, UiPlaylistItem, XtreamCluster, XtreamPlaylistItem,
    },
    utils::interner_gc,
};
use std::sync::Arc;
use tokio_stream::StreamExt;

pub(in crate::api::endpoints) async fn get_playlist_for_target(
    cfg_target: Option<&ConfigTarget>,
    cfg: &AppConfig,
    cluster: XtreamCluster,
    accept: Option<&str>,
) -> impl IntoResponse + Send {
    if let Some(target) = cfg_target {
        if target.has_output(TargetType::Xtream) {
            let Some(channel_iterator) = iter_raw_xtream_target_playlist(cfg, target, cluster).await else {
                return empty_json_list_response();
            };
            let item_filter = if cluster == XtreamCluster::Series {
                |pli: &XtreamPlaylistItem| {
                    !matches!(pli.item_type, PlaylistItemType::Series | PlaylistItemType::LocalSeries)
                }
            } else {
                |_pli: &XtreamPlaylistItem| true
            };
            let converted_stream = channel_iterator.filter(item_filter).map(UiPlaylistItem::from);
            return stream_json_or_bin_response_stream(accept, converted_stream).into_response();
        } else if target.has_output(TargetType::M3u) {
            let Some(channel_iterator) = iter_raw_m3u_target_playlist(cfg, target, Some(cluster)).await else {
                return empty_json_list_response();
            };
            let item_filter = if cluster == XtreamCluster::Series {
                |pli: &M3uPlaylistItem| {
                    !matches!(pli.item_type, PlaylistItemType::Series | PlaylistItemType::LocalSeries)
                }
            } else {
                |_pli: &M3uPlaylistItem| true
            };

            let converted_stream = channel_iterator.filter_map(move |res| match res {
                Ok(pli) => {
                    if item_filter(&pli) {
                        Some(UiPlaylistItem::from(pli))
                    } else {
                        None
                    }
                }
                Err(err) => {
                    warn!("Skipping unreadable M3U target playlist entry: {err}");
                    None
                }
            });
            return stream_json_or_bin_response_stream(accept, converted_stream).into_response();
        }
    }
    (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": "Invalid Arguments"}))).into_response()
}

pub(in crate::api::endpoints) async fn get_playlist_for_input(
    cfg_input: Option<&Arc<ConfigInput>>,
    cfg: &AppConfig,
    cluster: XtreamCluster,
    accept: Option<&str>,
) -> impl IntoResponse + Send {
    if let Some(input) = cfg_input {
        if matches!(input.input_type, InputType::Xtream | InputType::XtreamBatch) {
            let Some(channel_iterator) = iter_raw_xtream_input_playlist(cfg, input, cluster).await else {
                return empty_json_list_response();
            };
            let converted_stream = channel_iterator.map(UiPlaylistItem::from);
            return stream_json_or_bin_response_stream(accept, converted_stream).into_response();
        } else if matches!(input.input_type, InputType::M3u | InputType::M3uBatch) {
            let Some(channels) = iter_raw_m3u_input_playlist(cfg, input, Some(cluster)).await else {
                return empty_json_list_response();
            };
            let converted_stream = channels.filter_map(|res| match res {
                Ok(pli) => Some(UiPlaylistItem::from(pli)),
                Err(err) => {
                    warn!("Skipping unreadable M3U input playlist entry: {err}");
                    None
                }
            });
            return stream_json_or_bin_response_stream(accept, converted_stream).into_response();
        }
    }
    (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": "Invalid Arguments"}))).into_response()
}

pub(in crate::api::endpoints) async fn get_playlist_for_custom_provider(
    client: &reqwest::Client,
    cfg_input: Option<&Arc<ConfigInput>>,
    app_config: &Arc<AppConfig>,
    cluster: XtreamCluster,
    accept: Option<&str>,
) -> impl IntoResponse + Send {
    let cfg = app_config.config.load();
    match cfg_input {
        Some(input) => {
            let (result, errors) = match input.input_type {
                InputType::M3u | InputType::M3uBatch => {
                    m3u::download_m3u_playlist(app_config, client, &cfg, input).await
                }
                InputType::Xtream | InputType::XtreamBatch => {
                    let (pl, err, _) =
                        xtream::download_xtream_playlist(app_config, client, input, Some(&[cluster])).await;
                    (pl, err)
                }
                InputType::Library => {
                    return (
                        axum::http::StatusCode::BAD_REQUEST,
                        axum::Json(json!({ "error": "Library inputs are not supported on this endpoint"})),
                    )
                        .into_response();
                }
            };
            if result.is_empty() {
                let error_strings: Vec<String> = errors.iter().map(ToString::to_string).collect();
                (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": error_strings.join(", ")})))
                    .into_response()
            } else {
                let channels: Vec<UiPlaylistItem> =
                    result.iter().flat_map(|g| g.channels.iter()).map(UiPlaylistItem::from).collect();
                interner_gc();
                json_or_bin_response(accept, &channels).into_response()
            }
        }
        None => {
            (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": "Invalid Arguments"}))).into_response()
        }
    }
}
