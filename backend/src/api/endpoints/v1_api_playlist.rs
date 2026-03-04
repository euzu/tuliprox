use crate::{
    api::{
        api_utils::{create_api_proxy_user, json_or_bin_response},
        endpoints::{
            api_playlist_utils::{get_playlist_for_custom_provider, get_playlist_for_input, get_playlist_for_target},
            extract_accept_header::ExtractAcceptHeader,
            xmltv_api::serve_epg_web_ui,
            xtream_api::xtream_get_stream_info_response,
        },
        model::AppState,
    },
    auth::create_access_token,
    model::{parse_xmltv_for_web_ui_from_url, ConfigInput, ConfigInputFlags, ConfigInputFlagsSet, ConfigInputOptions},
    processing::processor::exec_processing,
    repository::xtream_get_item_for_stream_id,
};
use axum::{response::IntoResponse, Router};
use log::{debug, error};
use serde_json::json;
use shared::{
    model::{
        InputType, PlaylistEpgRequest, PlaylistRequest, ProxyType, TargetType, UiPlaylistItem, WebplayerUrlRequest,
        XtreamCluster,
    },
    utils::{sanitize_sensitive_info, Internable},
};
use std::sync::Arc;
use url::Url;
use shared::utils::deobfuscate_text;
use crate::api::api_utils::resource_response;

fn create_config_input_for_m3u(url: &str) -> ConfigInput {
    ConfigInput {
        id: 0,
        name: "m3u_req".intern(),
        input_type: InputType::M3u,
        url: String::from(url),
        enabled: true,
        options: Some(ConfigInputOptions {
            flags: ConfigInputFlagsSet::from_variants(&[
                ConfigInputFlags::XtreamLiveStreamUsePrefix,
                ConfigInputFlags::ResolveBackground,
            ]),
            resolve_delay: shared::utils::default_resolve_delay_secs(),
            probe_delay: shared::utils::default_probe_delay_secs(),
            probe_live_interval_hours: 120,
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
            flags: ConfigInputFlagsSet::from_variants(&[
                ConfigInputFlags::XtreamLiveStreamUsePrefix,
                ConfigInputFlags::ResolveBackground,
            ]),
            resolve_delay: shared::utils::default_resolve_delay_secs(),
            probe_delay: shared::utils::default_probe_delay_secs(),
            probe_live_interval_hours: 120,
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
            let provider_manager = Arc::clone(&app_state.active_provider);
            let disabled_headers = app_state.get_disabled_headers();
            let metadata_manager = Arc::clone(&app_state.metadata_manager);
            let update_guard = app_state.update_guard.clone();
            tokio::spawn({
                async move {
                    exec_processing(
                        &http_client,
                        app_config,
                        valid_targets,
                        Some(event_manager),
                        Some(app_state.clone()),
                        Some(playlist_state),
                        Some(update_guard),
                        disabled_headers,
                        Some(provider_manager),
                        Some(metadata_manager),
                        None,
                        None,
                    )
                    .await;
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
    let client = app_state.http_client.load();
    match playlist_req {
        PlaylistRequest::Target(target_id) => get_playlist_for_target(
            app_state.app_config.get_target_by_id(*target_id).as_deref(),
            app_state,
            cluster,
            accept.as_deref(),
        )
        .await
        .into_response(),
        PlaylistRequest::Input(input_id) => get_playlist_for_input(
            app_state.app_config.get_input_by_id(*input_id).as_ref(),
            app_state,
            cluster,
            accept.as_deref(),
        )
        .await
        .into_response(),
        PlaylistRequest::CustomXtream(xtream) => match Url::parse(&xtream.url) {
            Ok(parsed) if parsed.scheme() == "http" || parsed.scheme() == "https" => {
                let input = Arc::new(create_config_input_for_xtream(&xtream.username, &xtream.password, &xtream.url));
                get_playlist_for_custom_provider(
                    client.as_ref(),
                    Some(&input),
                    app_state,
                    cluster,
                    accept.as_deref(),
                )
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
                get_playlist_for_custom_provider(
                    client.as_ref(),
                    Some(&input),
                    app_state,
                    cluster,
                    accept.as_deref(),
                )
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

async fn playlist_series_info(
    axum::extract::Path((virtual_id, _provider_id)): axum::extract::Path<(String, String)>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(playlist_req): axum::extract::Json<PlaylistRequest>,
) -> impl IntoResponse + Send {
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
        }
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
    let server_name = config
        .web_ui
        .as_ref()
        .and_then(|web_ui| web_ui.player_server.as_ref())
        .map_or("default", |server_name| server_name.as_str());
    let server_info = app_state.app_config.get_server_info(server_name);
    let base_url = server_info.get_base_url();
    format!(
        "{base_url}/token/{access_token}/{}/{}/{}",
        playlist_item.target_id,
        playlist_item.cluster.as_stream_type(),
        playlist_item.virtual_id
    )
    .into_response()
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

async fn playlist_resource(
    req_headers: axum::http::HeaderMap,
    axum::extract::Path(resource): axum::extract::Path<String>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse + Send {
    let encrypt_secret = app_state.get_encrypt_secret();
    if let Ok(resource_url) = deobfuscate_text(&encrypt_secret, &resource) {
        resource_response(&app_state, &resource_url, &req_headers, None).await.into_response()
    } else {
        axum::http::StatusCode::BAD_REQUEST.into_response()
    }
}

pub fn v1_api_playlist_register_protected(router: Router<Arc<AppState>>) -> axum::Router<Arc<AppState>> {
    router
        .route("/playlist/webplayer", axum::routing::post(playlist_webplayer))
        .route("/playlist/update", axum::routing::post(playlist_update))
        .route("/playlist/epg", axum::routing::post(playlist_epg))
        .route("/playlist/live", axum::routing::post(playlist_content_live))
        .route("/playlist/vod", axum::routing::post(playlist_content_vod))
        .route("/playlist/series", axum::routing::post(playlist_content_series))
        .route("/playlist/series_info/{virtual_id}/{provider_id}", axum::routing::post(playlist_series_info))
        .route("/playlist/series/episode/{virtual_id}", axum::routing::post(playlist_episode_item))
}

pub fn v1_api_playlist_register_public(
    router: Router<Arc<AppState>>,
) -> axum::Router<Arc<AppState>> {
    router.route("/playlist/resource/{resource}", axum::routing::get(playlist_resource))
}

async fn playlist_episode_item(
    axum::extract::Path(virtual_id): axum::extract::Path<String>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(playlist_req): axum::extract::Json<PlaylistRequest>,
) -> impl IntoResponse + Send {
    if let PlaylistRequest::Target(target_id) = playlist_req {
        if let Some(target) = app_state.app_config.get_target_by_id(target_id) {
            if target.has_output(TargetType::Xtream) {
                if let Ok(vid) = virtual_id.parse::<u32>() {
                    if let Ok(pli) =
                        xtream_get_item_for_stream_id(vid, &app_state, &target, Some(XtreamCluster::Series)).await
                    {
                        return axum::Json(json!(UiPlaylistItem::from(pli))).into_response();
                    }
                }
            }
        }
    }
    axum::http::StatusCode::NO_CONTENT.into_response()
}
