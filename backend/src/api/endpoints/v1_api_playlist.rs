use crate::api::api_utils::{create_api_proxy_user, json_or_bin_response};
use crate::api::endpoints::api_playlist_utils::{get_playlist_for_custom_provider, get_playlist_for_input, get_playlist_for_target};
use crate::api::endpoints::extract_accept_header::ExtractAcceptHeader;
use crate::api::model::AppState;
use crate::auth::create_access_token;
use crate::model::{parse_xmltv_for_web_ui_from_url, ConfigInput, ConfigInputOptions};
use crate::processing::processor::playlist;
use axum::response::IntoResponse;
use axum::{Router};
use log::{debug, error};
use serde_json::json;
use shared::model::{InputType, PlaylistEpgRequest, PlaylistRequest, ProxyType, TargetType, WebplayerUrlRequest, XtreamCluster};
use shared::utils::{sanitize_sensitive_info, Internable};
use std::sync::Arc;
use url::Url;
use crate::api::endpoints::xmltv_api::{serve_epg_web_ui};
use crate::api::endpoints::xtream_api::xtream_get_stream_info_response;

fn create_config_input_for_m3u(url: &str) -> ConfigInput {
    ConfigInput {
        id: 0,
        name: "m3u_req".intern(),
        input_type: InputType::M3u,
        url: String::from(url),
        enabled: true,
        options: Some(ConfigInputOptions {
            xtream_skip_live: false,
            xtream_skip_vod: false,
            xtream_skip_series: false,
            xtream_live_stream_without_extension: false,
            xtream_live_stream_use_prefix: true,
        }),
        ..Default::default()
    }
}

fn create_config_input_for_xtream(username: &str, password: &str, host: &str) -> ConfigInput {
    ConfigInput {
        id: 0,
        name: "xc_req".intern(),
        input_type: InputType::Xtream,
        url: String::from(host),
        username: Some(String::from(username)),
        password: Some(String::from(password)),
        enabled: true,
        options: Some(ConfigInputOptions {
            xtream_skip_live: false,
            xtream_skip_vod: false,
            xtream_skip_series: false,
            xtream_live_stream_without_extension: false,
            xtream_live_stream_use_prefix: true,
        }),
        ..Default::default()
    }
}

async fn playlist_update(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(targets): axum::extract::Json<Vec<String>>,
) -> impl axum::response::IntoResponse + Send {
    let user_targets = if targets.is_empty() { None } else { Some(targets) };
    let process_targets = app_state.app_config.sources.load().validate_targets(user_targets.as_ref());
    match process_targets {
        Ok(valid_targets) => {
            let http_client = app_state.http_client.load().as_ref().clone();
            let app_config = Arc::clone(&app_state.app_config);
            let event_manager = Arc::clone(&app_state.event_manager);
            let playlist_state = Arc::clone(&app_state.playlists);
            let valid_targets = Arc::new(valid_targets);
            let disabled_headers = app_state.get_disabled_headers();
            tokio::spawn({
                async move {
                    playlist::exec_processing(&http_client, app_config, valid_targets, Some(event_manager),
                                              Some(playlist_state), Some(app_state.update_guard.clone()),
                                              disabled_headers).await;
                }
            });
            axum::http::StatusCode::ACCEPTED.into_response()
        }
        Err(err) => {
            error!("Failed playlist update {}", sanitize_sensitive_info(err.to_string().as_str()));
            (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": err.to_string()}))).into_response()
        }
    }
}

async fn playlist_content(
    accept: Option<String>,
    app_state: &Arc<AppState>,
    playlist_req: &PlaylistRequest,
    cluster: XtreamCluster,
) -> impl IntoResponse + Send {
    let _config = app_state.app_config.config.load();
    let client = app_state.http_client.load();
    match playlist_req {
        PlaylistRequest::Target(target_id) => {
            get_playlist_for_target(app_state.app_config.get_target_by_id(*target_id).as_deref(), &app_state.app_config, cluster, accept.as_deref()).await.into_response()
        }
        PlaylistRequest::Input(input_id) => {
            get_playlist_for_input(app_state.app_config.get_input_by_id(*input_id).as_ref(), &app_state.app_config, cluster, accept.as_deref()).await.into_response()
        }
        PlaylistRequest::CustomXtream(xtream) => {
            match Url::parse(&xtream.url) {
                Ok(parsed) if parsed.scheme() == "http" || parsed.scheme() == "https" => {
                    let input = Arc::new(create_config_input_for_xtream(&xtream.username, &xtream.password, &xtream.url));
                    get_playlist_for_custom_provider(client.as_ref(), Some(&input), &app_state.app_config, cluster, accept.as_deref()).await.into_response()
                }
                _ => {
                    (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": "Invalid url scheme; only http/https are allowed"}))).into_response()
                }
            }
        }
        PlaylistRequest::CustomM3u(m3u) => {
            match Url::parse(&m3u.url) {
                Ok(parsed) if parsed.scheme() == "http" || parsed.scheme() == "https" => {
                    let input = Arc::new(create_config_input_for_m3u(&m3u.url));
                    get_playlist_for_custom_provider(client.as_ref(), Some(&input), &app_state.app_config, cluster, accept.as_deref()).await.into_response()
                }
                _ => {
                    (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": "Invalid url scheme; only http/https are allowed"}))).into_response()
                }
            }
        }
    }
}

macro_rules! create_player_api_for_cluster {
    ($fn_name:ident, $cluster:expr) => {
        async fn $fn_name(
            ExtractAcceptHeader(accept): ExtractAcceptHeader,
            axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
            axum::extract::Json(playlist_req): axum::extract::Json<PlaylistRequest>,
        ) -> impl IntoResponse + Send {
            playlist_content(
                accept.clone(),
                &app_state,
                &playlist_req,
                $cluster
            )
            .await
            .into_response()
        }
    };
}

create_player_api_for_cluster!(playlist_content_live, XtreamCluster::Live);
create_player_api_for_cluster!(playlist_content_vod, XtreamCluster::Video);
create_player_api_for_cluster!(playlist_content_series, XtreamCluster::Series);

async fn playlist_series_info(
    axum::extract::Path((virtual_id, _provider_id)): axum::extract::Path<(
        String,
        String,
    )>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(playlist_req): axum::extract::Json<PlaylistRequest>,
) -> impl IntoResponse + Send {
    match playlist_req {
        PlaylistRequest::Target(target_id) => {
            if let Some(target) = app_state.app_config.get_target_by_id(target_id) {
                if target.has_output(TargetType::Xtream) {
                    let mut user = create_api_proxy_user(&app_state);
                    user.proxy = ProxyType::Redirect;
                    return xtream_get_stream_info_response(&app_state, &user, &target, &virtual_id, XtreamCluster::Series).await.into_response();
                }
            }
        }

        PlaylistRequest::Input(input_id) => {
            if let Some(input) = app_state.app_config.get_input_by_id(input_id) {
                if matches!(input.input_type, InputType::Xtream | InputType::XtreamBatch) {
                    // TODO: Implement series info retrieval for input-based requests
                    debug!("TODO: Implement series info retrieval for input-based requests");
                }
            }
        }
        PlaylistRequest::CustomXtream(_xtream) => {
            // TODO: Implement series info retrieval for custom Xtream requests
            debug!("TODO: Implement series info retrieval for custom Xtream requests");
        },
        PlaylistRequest::CustomM3u(_) => {}
    }
    axum::http::StatusCode::NO_CONTENT.into_response()
}

async fn playlist_webplayer(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(playlist_item): axum::extract::Json<WebplayerUrlRequest>,
) -> impl axum::response::IntoResponse + Send {
    let access_token = create_access_token(&app_state.app_config.access_token_secret, 30);
    let config = app_state.app_config.config.load();
    let server_name = config.web_ui.as_ref().and_then(|web_ui| web_ui.player_server.as_ref()).map_or("default", |server_name| server_name.as_str());
    let server_info = app_state.app_config.get_server_info(server_name);
    let base_url = server_info.get_base_url();
    format!("{base_url}/token/{access_token}/{}/{}/{}", playlist_item.target_id, playlist_item.cluster.as_stream_type(), playlist_item.virtual_id).into_response()
}

async fn playlist_epg(
    ExtractAcceptHeader(accept): ExtractAcceptHeader,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(playlist_epg_req): axum::extract::Json<PlaylistEpgRequest>,
) -> impl IntoResponse + Send {
    match playlist_epg_req {
        PlaylistEpgRequest::Target(target_id) => {
            if let Some(target) = app_state.app_config.get_target_by_id(target_id) {
                let config = &app_state.app_config.config.load();
                if let Some(epg_path) = crate::api::endpoints::xmltv_api::get_epg_path_for_target(config, &target) {
                    return serve_epg_web_ui(&app_state, accept.as_deref(), &epg_path, &target).await;
                }
            }
        }
        PlaylistEpgRequest::Input(_input_id) => {
            // TODO: This is currently not supported, because we could have multiple epg sources for one input
            //     if let Some(target) = app_state.app_config.get_input_by_id(input_id) {
            //         let config = &app_state.app_config.config.load();
            //         if let Some(epg_path) = crate::api::endpoints::xmltv_api::get_epg_path_for_input(config, &target)  {
            //             if let Ok(epg) = parse_xmltv_for_web_ui(&epg_path) {
            //                 return json_or_bin_response(accept.as_ref(), &epg).into_response();
            //             }
            //         }
            //     }
        }
        PlaylistEpgRequest::Custom(url) => {
            if let Ok(epg) = parse_xmltv_for_web_ui_from_url(&app_state, &url).await {
                return json_or_bin_response(accept.as_deref(), &epg).into_response();
            }
        }
    }
    axum::http::StatusCode::NO_CONTENT.into_response()
}

pub fn v1_api_playlist_register(router: Router<Arc<AppState>>) -> axum::Router<Arc<AppState>> {
    router
        .route("/playlist/webplayer", axum::routing::post(playlist_webplayer))
        .route("/playlist/update", axum::routing::post(playlist_update))
        .route("/playlist/epg", axum::routing::post(playlist_epg))
        .route("/playlist/live", axum::routing::post(playlist_content_live))
        .route("/playlist/vod", axum::routing::post(playlist_content_vod))
        .route("/playlist/series", axum::routing::post(playlist_content_series))
        .route("/playlist/series_info/{virtual_id}/{provider_id}", axum::routing::post(playlist_series_info))
}
