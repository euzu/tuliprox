use crate::model::{AppConfig, ConfigInput, ConfigTarget};
use crate::repository::{m3u_repository, xtream_repository};
use crate::utils::{m3u, xtream};
use axum::response::IntoResponse;
use serde_json::{json};
use shared::model::{CommonPlaylistItem, InputType, TargetType, XtreamCluster};
use std::sync::Arc;
use crate::api::api_utils::{empty_json_list_response, json_or_bin_response, stream_json_or_bin_response};
use shared::utils::interner_gc;

pub(in crate::api::endpoints) async fn get_playlist_for_target(cfg_target: Option<&ConfigTarget>, cfg: &AppConfig, cluster: XtreamCluster, accept: Option<&str>) -> impl IntoResponse + Send {
    if let Some(target) = cfg_target {
        if target.has_output(TargetType::Xtream) {
            let Some((_guard, channel_iterator)) = xtream_repository::iter_raw_xtream_target_playlist(cfg, target, cluster).await else {
              return empty_json_list_response();
            };
            let converted_iterator: Box<dyn Iterator<Item=CommonPlaylistItem> + Send> = Box::new(channel_iterator.map(CommonPlaylistItem::from));
            return stream_json_or_bin_response(accept, converted_iterator).into_response();
        } else if target.has_output(TargetType::M3u) {
            let Some((_guard, channel_iterator)) = m3u_repository::iter_raw_m3u_target_playlist(cfg, target, Some(cluster)).await else {
                return empty_json_list_response();
            };
            let converted_iterator: Box<dyn Iterator<Item=CommonPlaylistItem> + Send> = Box::new(channel_iterator.map(CommonPlaylistItem::from));
            return stream_json_or_bin_response(accept, converted_iterator).into_response();
        }
    }
    (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": "Invalid Arguments"}))).into_response()
}


pub(in crate::api::endpoints) async fn get_playlist_for_input(cfg_input: Option<&Arc<ConfigInput>>, cfg: &AppConfig, cluster: XtreamCluster, accept: Option<&str>) -> impl IntoResponse + Send {
    if let Some(input) = cfg_input {
        if matches!(input.input_type, InputType::Xtream | InputType::XtreamBatch) {
            let Some((_guard, channel_iterator)) = xtream_repository::iter_raw_xtream_input_playlist(cfg, input, cluster).await else {
                return empty_json_list_response();
            };
            let converted_iterator: Box<dyn Iterator<Item=CommonPlaylistItem> + Send> = Box::new(channel_iterator.map(CommonPlaylistItem::from));
            return stream_json_or_bin_response(accept, converted_iterator).into_response();
        } else if matches!(input.input_type, InputType::M3u | InputType::M3uBatch) {
            let Some((_guard, channels)) = m3u_repository::iter_raw_m3u_input_playlist(cfg, input, Some(cluster)).await else {
              return empty_json_list_response();
            };
            let converted_iterator: Box<dyn Iterator<Item=CommonPlaylistItem> + Send> = Box::new(channels.map(CommonPlaylistItem::from));
            return stream_json_or_bin_response(accept, converted_iterator).into_response();
        }
    }
    (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": "Invalid Arguments"}))).into_response()
}

pub(in crate::api::endpoints) async fn get_playlist_for_custom_provider(client: &reqwest::Client, cfg_input: Option<&Arc<ConfigInput>>, app_config: &Arc<AppConfig>, cluster: XtreamCluster, accept: Option<&str>) -> impl IntoResponse + Send {
    let cfg = app_config.config.load();
    match cfg_input {
        Some(input) => {
            let (result, errors) =
                match input.input_type {
                    InputType::M3u | InputType::M3uBatch => m3u::download_m3u_playlist(client, &cfg, input).await,
                    InputType::Xtream | InputType::XtreamBatch => {
                        let (pl, err, _) = xtream::download_xtream_playlist(app_config, client, input, Some(&[cluster])).await;
                        (pl, err)
                    }
                    InputType::Library => {
                        return (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({ "error": "Library inputs are not supported on this endpoint"}))).into_response();
                    }
                };
            if result.is_empty() {
                let error_strings: Vec<String> = errors.iter().map(ToString::to_string).collect();
                (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": error_strings.join(", ")}))).into_response()
            } else {
                let channels: Vec<CommonPlaylistItem> = result.iter().flat_map(|g| g.channels.iter()).map(CommonPlaylistItem::from).collect();
                interner_gc();
                json_or_bin_response(accept, &channels).into_response()
            }
        }
        None => (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": "Invalid Arguments"}))).into_response(),
    }
}
