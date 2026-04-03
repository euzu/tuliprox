use crate::api::model::AppState;
use crate::{
    api::api_utils::{empty_json_list_response, json_or_bin_response, stream_json_or_bin_response_stream},
    model::{ConfigInput, ConfigTarget},
    repository::{
        iter_raw_m3u_input_playlist, iter_raw_m3u_target_playlist, iter_raw_xtream_input_playlist,
        iter_raw_xtream_target_playlist,
    },
    utils::{m3u, xtream},
};
use axum::response::IntoResponse;
use log::warn;
use serde_json::json;
use shared::utils::{concat_path, concat_path_leading_slash, obfuscate_text, Internable};
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
    app_state: &Arc<AppState>,
    cluster: XtreamCluster,
    accept: Option<&str>,
) -> impl IntoResponse + Send {
    let config = app_state.app_config.config.load();
    let web_ui_path = config
        .web_ui
        .as_ref()
        .and_then(|w| w.path.as_ref())
        .map_or("", String::as_str);
    let resource_url = concat_path_leading_slash(web_ui_path, "api/v1/playlist/resource");
    let encrypt_secret = app_state.get_encrypt_secret();
    if let Some(target) = cfg_target {
        if target.has_output(TargetType::Xtream) {
            let Some(channel_iterator) = iter_raw_xtream_target_playlist(&app_state.app_config, target, cluster).await else {
                return empty_json_list_response();
            };
            let item_filter = if cluster == XtreamCluster::Series {
                |pli: &XtreamPlaylistItem| {
                    !matches!(pli.item_type, PlaylistItemType::Series | PlaylistItemType::LocalSeries)
                }
            } else {
                |_pli: &XtreamPlaylistItem| true
            };
            let converted_stream = channel_iterator.filter(item_filter).map(UiPlaylistItem::from).map(move |uiu| rewrite_resource_url(&encrypt_secret, &resource_url, uiu));
            return stream_json_or_bin_response_stream(accept, converted_stream).into_response();
        } else if target.has_output(TargetType::M3u) {
            let Some(channel_iterator) = iter_raw_m3u_target_playlist(&app_state.app_config, target, Some(cluster)).await else {
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
                        Some(rewrite_resource_url(&encrypt_secret, &resource_url, UiPlaylistItem::from(pli)))
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

fn rewrite_resource_url(encrypt_secret: &[u8; 16], resource_url: &str, item: UiPlaylistItem) -> UiPlaylistItem {
    if item.logo.is_empty() {
        return item;
    }
    let mut item = item;
    if item.logo.starts_with('/') {
        return item;
    }
    item.logo = concat_path(resource_url, &obfuscate_text(encrypt_secret, &item.logo)).intern();
    item
}

#[cfg(test)]
mod tests {
    use super::rewrite_resource_url;
    use shared::{
        model::{PlaylistItemType, UiPlaylistItem, XtreamCluster},
        utils::{obfuscate_text, Internable},
    };

    fn sample_item(logo: &str) -> UiPlaylistItem {
        UiPlaylistItem {
            virtual_id: 1,
            provider_id: "provider".intern(),
            name: "name".intern(),
            title: "title".intern(),
            group: "group".intern(),
            logo: logo.intern(),
            url: "file:///tmp/video.mkv".intern(),
            item_type: PlaylistItemType::Live,
            xtream_cluster: XtreamCluster::Live,
            category_id: 0,
            rating: 0.0,
            input_name: "test".intern(),
        }
    }

    #[test]
    fn rewrite_resource_url_keeps_internal_api_paths() {
        let secret = [7u8; 16];
        let item = sample_item("/api/v1/library/thumbnail/test-uuid");

        let rewritten = rewrite_resource_url(&secret, "/api/v1/playlist/resource", item);

        assert_eq!(rewritten.logo.as_ref(), "/api/v1/library/thumbnail/test-uuid");
    }

    #[test]
    fn rewrite_resource_url_wraps_external_urls() {
        let secret = [7u8; 16];
        let item = sample_item("https://example.com/poster.jpg");

        let rewritten = rewrite_resource_url(&secret, "/api/v1/playlist/resource", item);
        let expected_suffix = obfuscate_text(&secret, "https://example.com/poster.jpg");

        assert_eq!(
            rewritten.logo.as_ref(),
            format!("/api/v1/playlist/resource/{expected_suffix}")
        );
    }
}

pub(in crate::api::endpoints) async fn get_playlist_for_input(
    cfg_input: Option<&Arc<ConfigInput>>,
    app_state: &Arc<AppState>,
    cluster: XtreamCluster,
    accept: Option<&str>,
) -> impl IntoResponse + Send {
    if let Some(input) = cfg_input {
        if matches!(input.input_type, InputType::Xtream | InputType::XtreamBatch) {
            let Some(channel_iterator) = iter_raw_xtream_input_playlist(&app_state.app_config, input, cluster).await else {
                return empty_json_list_response();
            };
            let converted_stream = channel_iterator.map(UiPlaylistItem::from);
            return stream_json_or_bin_response_stream(accept, converted_stream).into_response();
        } else if matches!(input.input_type, InputType::M3u | InputType::M3uBatch) {
            let Some(channels) = iter_raw_m3u_input_playlist(&app_state.app_config, input, Some(cluster)).await else {
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
    app_state: &Arc<AppState>,
    cluster: XtreamCluster,
    accept: Option<&str>,
) -> impl IntoResponse + Send {
    let cfg = app_state.app_config.config.load();
    match cfg_input {
        Some(input) => {
            let (result, errors) = match input.get_download_input_type() {
                InputType::M3u | InputType::M3uBatch => {
                    m3u::download_m3u_playlist(&app_state.app_config, client, &cfg, input).await
                }
                InputType::Xtream | InputType::XtreamBatch => {
                    let (pl, err, _) =
                        xtream::download_xtream_playlist(&app_state.app_config, client, input, Some(&[cluster])).await;
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
