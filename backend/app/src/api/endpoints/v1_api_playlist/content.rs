use super::{create_config_input_for_m3u, create_config_input_for_xtream};
use crate::{
    api::{
        api_utils::{create_api_proxy_user, try_unwrap_body},
        endpoints::{
            api_playlist_utils::{
                get_playlist_for_custom_provider, get_playlist_for_input, get_playlist_for_target, rewrite_resource_url,
            },
            xtream_api::xtream_get_stream_info_response,
        },
        model::AppState,
    },
    iptv::xtream,
    model::InputSource,
    repository::xtream_get_item_for_stream_id,
};
use axum::response::IntoResponse;
use serde_json::json;
use shared::{
    model::{PlaylistRequest, ProxyType, TargetType, UiPlaylistItem, XtreamCluster},
    utils::{concat_path_leading_slash, Internable},
};
use std::sync::Arc;
use url::Url;

pub(super) async fn playlist_content(
    accept: Option<String>,
    app_state: &Arc<AppState>,
    playlist_req: &PlaylistRequest,
    cluster: XtreamCluster,
) -> impl IntoResponse + Send {
    let client = app_state.http_clients.default.load();
    match playlist_req {
        PlaylistRequest::Target(target_id) => get_playlist_for_target(
            app_state.app_config.get_target_by_id(*target_id).as_deref(),
            app_state,
            cluster,
            accept.as_deref(),
        )
        .await
        .into_response(),
        PlaylistRequest::Input(input_name) => get_playlist_for_input(
            app_state.app_config.get_input_by_name(&input_name.intern()).as_ref(),
            app_state,
            cluster,
            accept.as_deref(),
        )
        .await
        .into_response(),
        PlaylistRequest::CustomXtream(xtream) => match Url::parse(&xtream.url) {
            Ok(parsed) if parsed.scheme() == "http" || parsed.scheme() == "https" => {
                let input = Arc::new(create_config_input_for_xtream(&xtream.username, &xtream.password, &xtream.url));
                get_playlist_for_custom_provider(client.as_ref(), Some(&input), app_state, cluster, accept.as_deref())
                    .await
                    .into_response()
            }
            _ => (
                axum::http::StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": "Invalid url scheme; only http/https are allowed"})),
            )
                .into_response(),
        },
        PlaylistRequest::CustomM3u(m3u) => match Url::parse(&m3u.url) {
            Ok(parsed) if parsed.scheme() == "http" || parsed.scheme() == "https" => {
                let input = Arc::new(create_config_input_for_m3u(&m3u.url));
                get_playlist_for_custom_provider(client.as_ref(), Some(&input), app_state, cluster, accept.as_deref())
                    .await
                    .into_response()
            }
            _ => (
                axum::http::StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": "Invalid url scheme; only http/https are allowed"})),
            )
                .into_response(),
        },
    }
}

pub(super) async fn playlist_series_info(
    axum::extract::Path((virtual_id, provider_id)): axum::extract::Path<(String, String)>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(playlist_req): axum::extract::Json<PlaylistRequest>,
) -> impl IntoResponse + Send {
    let provider_id = provider_id.trim().parse::<u32>().ok();

    match playlist_req {
        PlaylistRequest::Target(target_id) => {
            if let Some(target) = app_state.app_config.get_target_by_id(target_id) {
                if target.has_output(TargetType::Xtream) {
                    let mut user = create_api_proxy_user(&app_state);
                    user.proxy = ProxyType::Redirect;
                    return xtream_get_stream_info_response(
                        &app_state,
                        &user,
                        &target,
                        &virtual_id,
                        XtreamCluster::Series,
                    )
                    .await
                    .into_response();
                }
            }
        }

        PlaylistRequest::Input(input_name) => {
            if let Some(input) = app_state.app_config.get_input_by_name(&input_name.intern()) {
                if input.input_type.is_xtream() {
                    // We cannot call `xtream_get_stream_info_response` directly here because that path
                    // depends on target-local virtual-id mapping (`xtream_get_item_for_stream_id`).
                    // Input/custom requests only provide provider_id, so we resolve series info from
                    // the upstream Xtream API using provider_id.
                    if let Some(provider_id) = provider_id {
                        if let Some(info_url) =
                            xtream::get_xtream_player_api_info_url(input.as_ref(), XtreamCluster::Series, provider_id)
                        {
                            let Ok(resolved_url) = input.resolve_url(&info_url) else {
                                return axum::http::StatusCode::NO_CONTENT.into_response();
                            };
                            let input_source = InputSource::from(input.as_ref()).with_url(resolved_url.to_string());
                            if let Ok(content) = xtream::get_xtream_stream_info_content(
                                &app_state.app_config,
                                &app_state.http_clients.default.load(),
                                &input_source,
                                false,
                            )
                            .await
                            {
                                return try_unwrap_body!(axum::response::Response::builder()
                                    .status(axum::http::StatusCode::OK)
                                    .header(axum::http::header::CONTENT_TYPE, mime::APPLICATION_JSON.to_string())
                                    .body(axum::body::Body::from(content)));
                            }
                        }
                    }
                }
            }
        }
        PlaylistRequest::CustomXtream(xtream_req) => {
            if let Some(provider_id) = provider_id {
                let input = create_config_input_for_xtream(&xtream_req.username, &xtream_req.password, &xtream_req.url);
                if let Some(info_url) =
                    xtream::get_xtream_player_api_info_url(&input, XtreamCluster::Series, provider_id)
                {
                    let input_source = InputSource::from(&input).with_url(info_url);
                    if let Ok(content) = xtream::get_xtream_stream_info_content(
                        &app_state.app_config,
                        &app_state.http_clients.default.load(),
                        &input_source,
                        false,
                    )
                    .await
                    {
                        return try_unwrap_body!(axum::response::Response::builder()
                            .status(axum::http::StatusCode::OK)
                            .header(axum::http::header::CONTENT_TYPE, mime::APPLICATION_JSON.to_string())
                            .body(axum::body::Body::from(content)));
                    }
                }
            }
        }
        PlaylistRequest::CustomM3u(_) => {}
    }
    axum::http::StatusCode::NO_CONTENT.into_response()
}

pub(super) async fn playlist_episode_item(
    axum::extract::Path(virtual_id): axum::extract::Path<String>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(playlist_req): axum::extract::Json<PlaylistRequest>,
) -> impl IntoResponse + Send {
    if let PlaylistRequest::Target(target_id) = playlist_req {
        if let Some(target) = app_state.app_config.get_target_by_id(target_id) {
            if target.has_output(TargetType::Xtream) {
                if let Ok(vid) = virtual_id.parse::<u32>() {
                    if let Ok(pli) = xtream_get_item_for_stream_id(
                        vid,
                        &app_state.app_config,
                        &app_state.playlists,
                        &target,
                        Some(XtreamCluster::Series),
                    )
                    .await
                    {
                        // The icon is wrapped like on every other Web UI path: the item carries the
                        // destination as it was stored, which may name a host internal to this instance.
                        let config = app_state.app_config.config.load();
                        let web_ui_path =
                            config.web_ui.as_ref().and_then(|web_ui| web_ui.path.as_ref()).map_or("", String::as_str);
                        let resource_url = concat_path_leading_slash(web_ui_path, "api/v1/playlist/resource");
                        drop(config);
                        let item = rewrite_resource_url(
                            &app_state.get_encrypt_secret(),
                            &resource_url,
                            UiPlaylistItem::from(pli),
                        );
                        return axum::Json(json!(item)).into_response();
                    }
                }
            }
        }
    }
    axum::http::StatusCode::NO_CONTENT.into_response()
}
