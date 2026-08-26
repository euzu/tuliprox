use crate::api::model::AppState;
use crate::{
    api::api_utils::{empty_json_list_response, stream_json_or_bin_response_stream, stream_json_or_bin_response_try_stream},
    iptv::{m3u, xtream},
    model::{ConfigInput, ConfigTarget},
    processing::processor::{download_stalker_playlist, StalkerCluster},
    repository::{
        iter_raw_m3u_input_playlist, iter_raw_m3u_target_playlist, iter_raw_xtream_input_playlist,
        iter_raw_xtream_target_playlist,
    },
};
use axum::response::IntoResponse;
use serde_json::json;
use shared::utils::{concat_path, concat_path_leading_slash, interner_gc, obfuscate_text, Internable};
use shared::model::{InputPersistence, M3uPlaylistItem, TargetType, UiPlaylistItem, XtreamCluster, XtreamPlaylistItem};
use std::sync::Arc;
use tokio_stream::StreamExt;
use crate::api::api_utils::json_or_bin_response;

pub(in crate::api::endpoints) const STALKER_RESOURCE_SCHEME: &str = "stalker://";

fn stalker_cluster(cluster: XtreamCluster) -> StalkerCluster {
    match cluster {
        XtreamCluster::Live => StalkerCluster::Live,
        XtreamCluster::Video => StalkerCluster::Vod,
        XtreamCluster::Series => StalkerCluster::Series,
    }
}

fn stalker_refresh_pending_response(accept: Option<&str>) -> axum::response::Response {
    let items: Vec<UiPlaylistItem> = Vec::new();
    let mut response = json_or_bin_response(accept, &items).into_response();
    response.headers_mut().insert(
        axum::http::HeaderName::from_static("x-tuliprox-refresh-state"),
        axum::http::HeaderValue::from_static("in-progress"),
    );
    response
}

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
                    !pli.item_type.is_series()
                }
            } else {
                |_pli: &XtreamPlaylistItem| true
            };
            let converted_stream = channel_iterator.filter_map(move |entry| match entry {
                Ok(item) if item_filter(&item) => Some(Ok(rewrite_resource_url(
                    &encrypt_secret,
                    &resource_url,
                    UiPlaylistItem::from(item),
                ))),
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            });
            return stream_json_or_bin_response_try_stream(accept, converted_stream).into_response();
        } else if target.has_output(TargetType::M3u) {
            let Some(channel_iterator) = iter_raw_m3u_target_playlist(&app_state.app_config, target, Some(cluster)).await else {
                return empty_json_list_response();
            };
            let item_filter = if cluster == XtreamCluster::Series {
                |pli: &M3uPlaylistItem| {
                    !pli.item_type.is_series()
                }
            } else {
                |_pli: &M3uPlaylistItem| true
            };

            let converted_stream = channel_iterator.filter_map(move |res| match res {
                Ok(pli) if item_filter(&pli) => Some(Ok(rewrite_resource_url(
                    &encrypt_secret,
                    &resource_url,
                    UiPlaylistItem::from(pli),
                ))),
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            });
            return stream_json_or_bin_response_try_stream(accept, converted_stream).into_response();
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

fn rewrite_stalker_playback_url(
    encrypt_secret: &[u8; 16],
    resource_url: &str,
    input_id: u16,
    mut item: UiPlaylistItem,
) -> UiPlaylistItem {
    let locator = format!(
        "{STALKER_RESOURCE_SCHEME}{input_id}/{}/{}",
        item.xtream_cluster.as_stream_type(),
        item.provider_id
    );
    item.url = concat_path(resource_url, &obfuscate_text(encrypt_secret, &locator)).intern();
    item
}

pub(in crate::api::endpoints) async fn get_playlist_for_input(
    cfg_input: Option<&Arc<ConfigInput>>,
    app_state: &Arc<AppState>,
    cluster: XtreamCluster,
    accept: Option<&str>,
) -> impl IntoResponse + Send {
    if let Some(input) = cfg_input {
        if input.input_type.is_xtream() {
            let Some(channel_iterator) = iter_raw_xtream_input_playlist(&app_state.app_config, input, cluster).await else {
                return empty_json_list_response();
            };
            let converted_stream = channel_iterator.map(|entry| entry.map(UiPlaylistItem::from));
            return stream_json_or_bin_response_try_stream(accept, converted_stream).into_response();
        } else if input.input_type.is_m3u() {
            let Some(channels) = iter_raw_m3u_input_playlist(&app_state.app_config, input, Some(cluster)).await else {
                return empty_json_list_response();
            };
            let converted_stream = channels.map(|entry| entry.map(UiPlaylistItem::from));
            return stream_json_or_bin_response_try_stream(accept, converted_stream).into_response();
        } else if input.input_type.is_stalker() {
            // TODO refactor
            let stalker_cluster = stalker_cluster(cluster);
            let client = app_state.http_client.load();
            let (groups, errors, _, partial) =
                download_stalker_playlist(
                    &app_state.app_config,
                    client.as_ref(),
                    input,
                    Some(&[stalker_cluster]),
                    crate::processing::processor::StalkerRefreshMode::ServerSlice,
                    true,
                )
                .await;
            if groups.is_empty() {
                if partial {
                    return stalker_refresh_pending_response(accept);
                }
                if errors.is_empty() {
                    return json_or_bin_response(accept, &Vec::<UiPlaylistItem>::new()).into_response();
                }
                let error_strings: Vec<String> = errors.iter().map(ToString::to_string).collect();
                return (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": error_strings.join(", ")})))
                    .into_response();
            }
            let config = app_state.app_config.config.load();
            let web_ui_path = config.web_ui.as_ref().and_then(|web_ui| web_ui.path.as_ref()).map_or("", String::as_str);
            let resource_url = concat_path_leading_slash(web_ui_path, "api/v1/playlist/resource");
            let encrypt_secret = app_state.get_encrypt_secret();
            let channels: Vec<UiPlaylistItem> = groups
                .iter()
                .flat_map(|group| group.channels.iter())
                .map(UiPlaylistItem::from)
                .map(|item| rewrite_stalker_playback_url(&encrypt_secret, &resource_url, input.id, item))
                .collect();
            interner_gc();
            return json_or_bin_response(accept, &channels).into_response();
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
            let (result, errors, partial) = match input.get_download_input_type().persistence() {
                InputPersistence::M3u => {
                    let (playlist, errors) =
                        m3u::download_m3u_playlist(&app_state.app_config, client, &cfg, input).await;
                    (playlist, errors, false)
                }
                InputPersistence::Xtream => {
                    let (pl, err, _) =
                        xtream::download_xtream_playlist(&app_state.app_config, client, input, Some(&[cluster])).await;
                    (pl, err, false)
                }
                InputPersistence::Library => {
                    return (
                        axum::http::StatusCode::BAD_REQUEST,
                        axum::Json(json!({ "error": "Library inputs are not supported on this endpoint"})),
                    )
                        .into_response();
                }
                InputPersistence::MediaServer => {
                    return (
                        axum::http::StatusCode::BAD_REQUEST,
                        axum::Json(json!({ "error": "Media-server inputs are not supported on this endpoint yet"})),
                    )
                        .into_response();
                }
                InputPersistence::Stalker => {
                    let stalker_cluster = stalker_cluster(cluster);
                    // The Stalker processor mirrors the M3U/Xtream path: it returns
                    // `PlaylistGroup`s for the requested cluster only. The third
                    // tuple element (`use_disk_based_processing`) is irrelevant for
                    // a live preview — we always surface the in-memory items here.
                    let (groups, errs, _, partial) = download_stalker_playlist(
                        &app_state.app_config,
                        client,
                        input,
                        Some(&[stalker_cluster]),
                        crate::processing::processor::StalkerRefreshMode::ServerSlice,
                        true,
                    )
                    .await;
                    (groups, errs, partial)
                }
            };
            if result.is_empty() {
                if partial {
                    return stalker_refresh_pending_response(accept);
                }
                if errors.is_empty() {
                    return json_or_bin_response(accept, &Vec::<UiPlaylistItem>::new()).into_response();
                }
                let error_strings: Vec<String> = errors.iter().map(ToString::to_string).collect();
                (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": error_strings.join(", ")})))
                    .into_response()
            } else {
                // Stream the UI conversion lazily (like the target/input endpoints) instead of
                // collecting the whole playlist into a second Vec and serializing it all at once.
                let web_ui_path = cfg.web_ui.as_ref().and_then(|web_ui| web_ui.path.as_ref()).map_or("", String::as_str);
                let resource_url = concat_path_leading_slash(web_ui_path, "api/v1/playlist/resource");
                let encrypt_secret = app_state.get_encrypt_secret();
                let input_id = input.id;
                let converted_stream = tokio_stream::iter(result.into_iter().flat_map(|g| g.channels).map(move |pli| {
                    rewrite_stalker_playback_url(
                        &encrypt_secret,
                        &resource_url,
                        input_id,
                        UiPlaylistItem::from(&pli),
                    )
                }));
                stream_json_or_bin_response_stream(accept, converted_stream).into_response()
            }
        }
        None => {
            (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": "Invalid Arguments"}))).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{rewrite_resource_url, stalker_refresh_pending_response};
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
            epg_channel_id: None,
        }
    }

    #[test]
    fn pending_stalker_preview_keeps_array_contract_and_marks_progress() {
        let response = stalker_refresh_pending_response(None);
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        assert_eq!(
            response.headers().get("x-tuliprox-refresh-state").and_then(|value| value.to_str().ok()),
            Some("in-progress")
        );
    }

    #[test]
    fn rewrite_resource_url_keeps_internal_api_paths() {
        let secret = [7u8; 16];
        let item = sample_item("/api/v1/library/thumbnail/test-uuid");

        let rewritten = rewrite_resource_url(&secret, "/api/v1/playlist/resource", item);

        assert_eq!(rewritten.logo.as_ref(), "/api/v1/library/thumbnail/test-uuid");
    }

    #[test]
    fn rewrite_resource_url_returns_unchanged_for_empty_logo() {
        let secret = [7u8; 16];
        let item = sample_item("");

        let rewritten = rewrite_resource_url(&secret, "/api/v1/playlist/resource", item);

        assert_eq!(rewritten.logo.as_ref(), "");
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
