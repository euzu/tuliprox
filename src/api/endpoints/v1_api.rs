use crate::api::endpoints::api_playlist_utils::{get_playlist, get_playlist_for_target};
use crate::api::endpoints::download_api;
use crate::api::endpoints::user_api::user_api_register;
use crate::api::model::app_state::AppState;
use crate::api::model::config::{ServerConfig, ServerInputConfig, ServerSourceConfig, ServerTargetConfig};
use crate::api::model::request::{PlaylistRequest, PlaylistRequestType};
use crate::auth::access_token::create_access_token;
use crate::auth::authenticator::validator_admin;
use crate::tuliprox_error::TuliproxError;
use crate::model::StatusCheck;
use crate::model::XtreamPlaylistItem;
use crate::model::{validate_targets, Config, ConfigDto, ConfigInput, ConfigInputOptions, ConfigSource, ConfigTarget, InputType};
use crate::model::{ApiProxyConfig, ApiProxyServerInfo, ProxyUserCredentials, TargetUser};
use crate::processing::processor::playlist;
use crate::repository::user_repository::store_api_user;
use crate::utils::ip_checker::get_ips;
use crate::utils::request::sanitize_sensitive_info;
use crate::{utils, VERSION};
use axum::response::IntoResponse;
use log::error;
use serde_json::json;
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

fn intern_save_config_api_proxy(backup_dir: &str, api_proxy: &ApiProxyConfig, file_path: &str) -> Option<TuliproxError> {
    match utils::save_api_proxy(file_path, backup_dir, api_proxy) {
        Ok(()) => {}
        Err(err) => {
            error!("Failed to save api_proxy.yml {err}");
            return Some(err);
        }
    }
    None
}

fn intern_save_config_main(file_path: &str, backup_dir: &str, cfg: &ConfigDto) -> Option<TuliproxError> {
    match utils::save_main_config(file_path, backup_dir, cfg) {
        Ok(()) => {}
        Err(err) => {
            error!("Failed to save config.yml {err}");
            return Some(err);
        }
    }
    None
}

async fn save_config_api_proxy_user(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(mut users): axum::extract::Json<Vec<TargetUser>>,
) -> impl axum::response::IntoResponse + Send {
    let mut usernames = HashSet::new();
    let mut tokens = HashSet::new();
    for target_user in &mut users {
        for credential in &mut target_user.credentials {
            credential.trim();
            if let Err(err) = credential.validate() {
                return (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": err.to_string()}))).into_response();
            }
            if usernames.contains(&credential.username) {
                return (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": format!("Duplicate username {}", &credential.username)}))).into_response();
            }
            usernames.insert(&credential.username);
            if let Some(token) = &credential.token {
                if tokens.contains(token) {
                    return (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": format!("Duplicate token {token}")}))).into_response();
                }
                tokens.insert(token);
            }
        }
    }

    if let Some(old_api_proxy) =  app_state.config.t_api_proxy.load().clone() {
        let mut api_proxy = (*old_api_proxy).clone();
        api_proxy.user = users;
        api_proxy.user.iter_mut().flat_map(|t| &mut t.credentials).for_each(ProxyUserCredentials::prepare);
        let new_api_proxy = Arc::new(api_proxy);
        app_state.config.t_api_proxy.store(Some(Arc::clone(&new_api_proxy)));

        if new_api_proxy.use_user_db {
            if let Err(err) = store_api_user(&app_state.config, &new_api_proxy.user) {
                return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, axum::Json(json!({"error": err.to_string()}))).into_response();
            }
        } else {
            let backup_dir = app_state.config.backup_dir.as_ref().unwrap().as_str();
            if let Some(err) = intern_save_config_api_proxy(backup_dir, &new_api_proxy, app_state.config.t_api_proxy_file_path.as_str()) {
                return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, axum::Json(json!({"error": err.to_string()}))).into_response();
            }
        }
    }
    axum::http::StatusCode::OK.into_response()
}

async fn save_config_main(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(cfg): axum::extract::Json<ConfigDto>,
) -> impl axum::response::IntoResponse + Send {
    if cfg.is_valid() {
        let file_path = app_state.config.t_config_file_path.as_str();
        let backup_dir = app_state.config.backup_dir.as_ref().unwrap().as_str();
        if let Some(err) = intern_save_config_main(file_path, backup_dir, &cfg) {
            return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, axum::Json(json!({"error": err.to_string()}))).into_response();
        }
        axum::http::StatusCode::OK.into_response()
    } else {
        (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": "Invalid content"}))).into_response()
    }
}

async fn save_config_api_proxy_config(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(mut req_api_proxy): axum::extract::Json<Vec<ApiProxyServerInfo>>,
) -> impl axum::response::IntoResponse + Send {
    for server_info in &mut req_api_proxy {
        if !server_info.validate() {
            return (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": "Invalid content"}))).into_response();
        }
    }

    // TODO wenn hot reload an ist wird doppelt geladen
    if let Some(old_api_proxy) =  app_state.config.t_api_proxy.load().clone() {
        let mut api_proxy = (*old_api_proxy).clone();
        api_proxy.server = req_api_proxy;
        let new_api_proxy = Arc::new(api_proxy);
        app_state.config.t_api_proxy.store(Some(Arc::clone(&new_api_proxy)));
        let backup_dir = app_state.config.backup_dir.as_ref().unwrap().as_str();
        if let Some(err) = intern_save_config_api_proxy(backup_dir, new_api_proxy.as_ref(), app_state.config.t_api_proxy_file_path.as_str()) {
            return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, axum::Json(json!({"error": err.to_string()}))).into_response();
        }
    }
    axum::http::StatusCode::OK.into_response()
}

async fn playlist_update(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(targets): axum::extract::Json<Vec<String>>,
) -> impl axum::response::IntoResponse + Send {
    let user_targets = if targets.is_empty() { None } else { Some(targets) };
    let process_targets = validate_targets(user_targets.as_ref(), &app_state.config.sources);
    match process_targets {
        Ok(valid_targets) => {
            tokio::spawn(playlist::exec_processing(Arc::clone(&app_state.http_client), Arc::clone(&app_state.config), Arc::new(valid_targets)));
            axum::http::StatusCode::OK.into_response()
        }
        Err(err) => {
            error!("Failed playlist update {}", sanitize_sensitive_info(err.to_string().as_str()));
            (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": err.to_string()}))).into_response()
        }
    }
}

fn create_config_input_for_m3u(url: &str) -> ConfigInput {
    ConfigInput {
        id: 0,
        name: String::from("m3u_req"),
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
        name: String::from("xc_req"),
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


async fn playlist_content(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(playlist_req): axum::extract::Json<PlaylistRequest>,
) -> impl IntoResponse + Send {
    match playlist_req.rtype {
        PlaylistRequestType::Input => {
            if let Some(source_id) = playlist_req.source_id {
                get_playlist(Arc::clone(&app_state.http_client), app_state.config.get_input_by_id(source_id), &app_state.config).await.into_response()
            } else {
                (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": "Invalid input"}))).into_response()
            }
        }
        PlaylistRequestType::Target => {
            if let Some(source_id) = playlist_req.source_id {
                get_playlist_for_target(app_state.config.get_target_by_id(source_id), &app_state.config).await.into_response()
            } else {
                (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": "Invalid target"}))).into_response()
            }
        }
        PlaylistRequestType::Xtream => {
            if let (Some(url), Some(username), Some(password)) = (playlist_req.url.as_ref(), playlist_req.username.as_ref(), playlist_req.password.as_ref()) {
                let input = create_config_input_for_xtream(username, password, url);
                get_playlist(Arc::clone(&app_state.http_client), Some(&input), &app_state.config).await.into_response()
            } else {
                (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": "Invalid url"}))).into_response()
            }
        }
        PlaylistRequestType::M3U => {
            if let Some(url) = playlist_req.url.as_ref() {
                let input = create_config_input_for_m3u(url);
                get_playlist(Arc::clone(&app_state.http_client), Some(&input), &app_state.config).await.into_response()
            } else {
                (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": "Invalid url"}))).into_response()
            }
        }
    }
}

async fn playlist_webplayer(
    axum::extract::Path(target_id): axum::extract::Path<u32>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(playlist_item): axum::extract::Json<XtreamPlaylistItem>,
) -> impl axum::response::IntoResponse + Send {
    let access_token = create_access_token(&app_state.config.t_access_token_secret, 5);
    let server_name = app_state.config.web_ui.as_ref().and_then(|web_ui| web_ui.player_server.as_ref()).map_or("default", |server_name| server_name.as_str());
    let server_info = app_state.config.get_server_info(server_name);
    let base_url = server_info.get_base_url();
    format!("{base_url}/token/{access_token}/{target_id}/{}/{}", playlist_item.xtream_cluster.as_stream_type(), playlist_item.virtual_id).into_response()
}

async fn config(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl axum::response::IntoResponse + Send {
    let map_input = |i: &ConfigInput| ServerInputConfig {
        id: i.id,
        name: i.name.clone(),
        input_type: i.input_type,
        url: i.url.clone(),
        username: i.username.clone(),
        password: i.password.clone(),
        persist: i.persist.clone(),
        enabled: i.enabled,
    };

    let map_target = |t: &ConfigTarget| ServerTargetConfig {
        id: t.id,
        enabled: t.enabled,
        name: t.name.clone(),
        options: t.options.clone(),
        sort: t.sort.clone(),
        filter: t.filter.clone(),
        output: t.output.clone(),
        rename: t.rename.clone(),
        mapping: t.mapping.clone(),
        processing_order: t.processing_order.clone(),
        watch: t.watch.clone(),
    };

    let map_source = |s: &ConfigSource| ServerSourceConfig {
        inputs: s.inputs.iter().map(map_input).collect(),
        targets: s.targets.iter().map(map_target).collect(),
    };

    let map_config = |config: &Config| ServerConfig {
        api: config.api.clone(),
        threads: config.threads,
        working_dir: config.working_dir.clone(),
        backup_dir: config.backup_dir.clone(),
        user_config_dir: config.user_config_dir.clone(),
        log: config.log.clone(),
        update_on_boot: config.update_on_boot,
        web_ui: config.web_ui.clone(),
        schedules: config.schedules.clone(),
        reverse_proxy: config.reverse_proxy.clone(),
        messaging: config.messaging.clone(),
        video: config.video.clone(),
        sources: config.sources.iter().map(map_source).collect(),
        proxy: config.proxy.clone(),
        ipcheck: config.ipcheck.clone(),
        api_proxy: utils::read_api_proxy(&app_state.config, false),
    };

    let mut result = match utils::read_config(app_state.config.t_config_path.as_str(),
                                                      app_state.config.t_config_file_path.as_str(),
                                                      app_state.config.t_sources_file_path.as_str(),
                                                      app_state.config.t_api_proxy_file_path.as_str(),
                                                      Some(app_state.config.t_mapping_file_path.to_string()),
                                                      false) {
        Ok(mut cfg) => {
            let _ = cfg.prepare(false);
            map_config(&cfg)
        }
        Err(_) => map_config(&app_state.config)
    };

    // if we didn't read it from file then we should use it from app_state
    if result.api_proxy.is_none() {
        let value = app_state.config.t_api_proxy.load();
        result.api_proxy = value.as_ref().map(|a| a.as_ref().clone());
    }

    axum::response::Json(result).into_response()
}


#[derive(Debug, serde::Serialize, serde::Deserialize, Default)]
struct IpCheck {
    #[serde(skip_serializing_if = "Option::is_none")]
    ipv4: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ipv6: Option<String>,
}

async fn create_ipinfo_check(app_state: &Arc<AppState>) -> Option<(Option<String>, Option<String>)> {
    if let Some(ipcheck) = app_state.config.ipcheck.as_ref() {
        if let Ok(check) = get_ips(&app_state.http_client, ipcheck).await {
            return Some(check);
        }
    }
    None
}

async fn create_status_check(app_state: &Arc<AppState>) -> StatusCheck {
    let cache = match app_state.cache.as_ref().as_ref() {
        None => None,
        Some(lock) => {
            Some(lock.lock().await.get_size_text())
        }
    };
    let (active_users, active_user_connections) = {
        let active_user = &app_state.active_users;
        (active_user.active_users().await, active_user.active_connections().await)
    };

    let active_provider_connections = app_state.active_provider.active_connections().await.map(|c| c.into_iter().collect::<BTreeMap<_, _>>());

    StatusCheck {
        status: "ok".to_string(),
        version: VERSION.to_string(),
        build_time: crate::api::api_utils::get_build_time(),
        server_time: crate::api::api_utils::get_server_time(),
        memory: crate::api::api_utils::get_memory_usage(),
        active_users,
        active_user_connections,
        active_provider_connections,
        cache,
    }
}
async fn status(axum::extract::State(app_state): axum::extract::State<Arc<AppState>>) -> axum::response::Response {
    let status = create_status_check(&app_state).await;
    match serde_json::to_string_pretty(&status) {
        Ok(pretty_json) => axum::response::Response::builder().status(axum::http::StatusCode::OK)
            .header(axum::http::header::CONTENT_TYPE, mime::APPLICATION_JSON.to_string()).body(pretty_json).unwrap().into_response(),
        Err(_) => axum::Json(status).into_response(),
    }
}

async fn ipinfo(axum::extract::State(app_state): axum::extract::State<Arc<AppState>>) -> axum::response::Response {
    if let Some((ipv4, ipv6)) = create_ipinfo_check(&app_state).await {
        let ipcheck = IpCheck {
            ipv4,
            ipv6,
        };
        return  match serde_json::to_string(&ipcheck) {
            Ok(json) => axum::response::Response::builder().status(axum::http::StatusCode::OK)
                .header(axum::http::header::CONTENT_TYPE, mime::APPLICATION_JSON.to_string()).body(json).unwrap().into_response(),
            Err(_) => axum::Json(ipcheck).into_response(),
        }
    }
    axum::http::StatusCode::BAD_REQUEST.into_response()
}


pub fn v1_api_register(web_auth_enabled: bool, app_state: Arc<AppState>, web_ui_path: &str) -> axum::Router<Arc<AppState>> {
    let mut router = axum::Router::new();
    router = router
        .route("/status", axum::routing::get(status))
        .route("/config", axum::routing::get(config))
        .route("/config/main", axum::routing::post(save_config_main))
        .route("/config/user", axum::routing::post(save_config_api_proxy_user))
        .route("/config/apiproxy", axum::routing::post(save_config_api_proxy_config))
        .route("/playlist/webplayer/{target_id}", axum::routing::post(playlist_webplayer))
        .route("/playlist/update", axum::routing::post(playlist_update))
        .route("/playlist", axum::routing::post(playlist_content))
        .route("/file/download", axum::routing::post(download_api::queue_download_file))
        .route("/file/download/info", axum::routing::get(download_api::download_file_info));
    if app_state.config.ipcheck.is_some() {
        router = router.route("/ipinfo", axum::routing::get(ipinfo));
    }
    if web_auth_enabled {
        router = router.route_layer(axum::middleware::from_fn_with_state(Arc::clone(&app_state), validator_admin));
    }

    let mut base_router = axum::Router::new();
    if app_state.config.web_ui.as_ref().is_none_or(|c| c.user_ui_enabled) {
        base_router = base_router.merge(user_api_register(app_state));
    }
    base_router.nest(&format!("{web_ui_path}/api/v1"), router)
}
