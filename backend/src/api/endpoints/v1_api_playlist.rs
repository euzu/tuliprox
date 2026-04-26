use crate::api::api_utils::resource_response;
use crate::{
    api::{
        api_utils::{create_api_proxy_user, json_or_bin_response},
        endpoints::{
            api_playlist_utils::{get_playlist_for_custom_provider, get_playlist_for_input, get_playlist_for_target},
            extract_accept_header::ExtractAcceptHeader,
            xmltv_api::{rewrite_epg_channel_resource_url, serve_epg_web_ui, stream_epg_api},
            xtream_api::xtream_get_stream_info_response,
        },
        model::AppState,
    },
    auth::{create_access_token, permission_layer},
    model::{
        AppConfig, ConfigInput, ConfigInputFlags, ConfigInputOptions, parse_xmltv_for_web_ui_from_file,
        parse_xmltv_for_web_ui_from_url,
    },
    repository::xtream_get_item_for_stream_id,
    utils::{epg::get_input_raw_epg_file_path, file_exists_async},
};
use axum::{response::IntoResponse, Router};
use log::{debug, error};
use serde_json::json;
use shared::utils::deobfuscate_text;
use shared::{
    model::{
        EpgChannel, EpgProgramme, InputType, PlaylistEpgRequest, PlaylistRequest, PlaylistUrlResolveRequest, ProxyType,
        TargetType, UiPlaylistItem, XtreamCluster, permission::Permission,
    },
    utils::{Internable, concat_path_leading_slash, sanitize_sensitive_info},
};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use url::Url;

fn create_config_input_for_m3u(url: &str) -> ConfigInput {
    ConfigInput {
        id: 0,
        name: "m3u_req".intern(),
        input_type: InputType::M3u,
        url: String::from(url),
        enabled: true,
        options: Some(ConfigInputOptions {
            flags: ConfigInputFlags::XtreamLiveStreamUsePrefix | ConfigInputFlags::ResolveBackground,
            resolve_delay: shared::utils::default_resolve_delay_secs(),
            probe_delay: shared::utils::default_probe_delay_secs(),
            probe_live_interval_hours: 120,
            resolve_filter: None,
            probe_filter: None,
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
            flags: ConfigInputFlags::XtreamLiveStreamUsePrefix | ConfigInputFlags::ResolveBackground,
            resolve_delay: shared::utils::default_resolve_delay_secs(),
            probe_delay: shared::utils::default_probe_delay_secs(),
            probe_live_interval_hours: 120,
            resolve_filter: None,
            probe_filter: None,
        }),
        ..Default::default()
    }
}

fn resolve_provider_url_with_input(input: &ConfigInput, url: &str) -> String {
    match input.resolve_url(url) {
        Ok(resolved) => resolved.into_owned(),
        Err(err) => {
            let sanitized_url = sanitize_sensitive_info(url);
            let err_text = err.to_string();
            let sanitized_err = sanitize_sensitive_info(&err_text);
            error!("resolve_provider_url_with_input failed for url '{sanitized_url}': {sanitized_err}");
            url.to_string()
        }
    }
}

fn resolve_provider_url_for_request(app_config: &AppConfig, playlist_request: &PlaylistRequest, url: &str) -> String {
    if !url.starts_with(shared::utils::PROVIDER_SCHEME_PREFIX) {
        return url.to_string();
    }

    match playlist_request {
        PlaylistRequest::Input(input_name) => app_config
            .get_input_by_name(&input_name.intern())
            .map_or_else(|| url.to_string(), |input| resolve_provider_url_with_input(input.as_ref(), url)),
        PlaylistRequest::Target(target_id) => app_config
            .get_target_by_id(*target_id)
            .and_then(|target| app_config.get_inputs_for_target(&target.name))
            .and_then(|inputs| {
                let mut matches = inputs.into_iter().filter(|input| input.get_resolve_provider(url).is_some());
                let first = matches.next()?;
                if matches.next().is_some() {
                    return None;
                }
                Some(resolve_provider_url_with_input(first.as_ref(), url))
            })
            .unwrap_or_else(|| url.to_string()),
        PlaylistRequest::CustomXtream(_) | PlaylistRequest::CustomM3u(_) => url.to_string(),
    }
}

fn build_playlist_webplayer_url(
    base_url: &str,
    access_token: &str,
    target_id: u16,
    virtual_id: u32,
    cluster: XtreamCluster,
) -> String {
    format!("{base_url}/token/{access_token}/{}/{}/{}", target_id, cluster.as_stream_type(), virtual_id)
}

#[derive(Hash, Eq, PartialEq)]
struct WebUiProgrammeKey {
    start: i64,
    stop: i64,
}

impl From<&EpgProgramme> for WebUiProgrammeKey {
    fn from(programme: &EpgProgramme) -> Self {
        Self { start: programme.start, stop: programme.stop }
    }
}

struct WebUiChannelAcc {
    priority: i16,
    channel: EpgChannel,
    programmes: HashSet<WebUiProgrammeKey>,
}

fn merge_missing_web_ui_programmes<I>(
    channel: &mut EpgChannel,
    programmes: &mut HashSet<WebUiProgrammeKey>,
    incoming: I,
) where
    I: IntoIterator<Item = EpgProgramme>,
{
    for programme in incoming {
        let key = WebUiProgrammeKey::from(&programme);
        if programmes.insert(key) {
            channel.programmes.push(programme);
        }
    }
    channel.programmes.sort_by_key(|programme| programme.start);
}

fn merge_epg_channels(mut channels_by_source: Vec<(i16, Vec<EpgChannel>)>) -> Vec<EpgChannel> {
    let mut merged: HashMap<Arc<str>, WebUiChannelAcc> = HashMap::new();
    channels_by_source.sort_by_key(|(priority, _)| *priority);

    for (priority, channels) in channels_by_source.drain(..) {
        for channel in channels {
            match merged.entry(channel.id.clone()) {
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    let acc = entry.get_mut();
                    if priority < acc.priority {
                        let previous_programmes = std::mem::replace(&mut acc.channel, channel).programmes;
                        acc.priority = priority;
                        acc.programmes =
                            acc.channel.programmes.iter().map(WebUiProgrammeKey::from).collect::<HashSet<_>>();
                        merge_missing_web_ui_programmes(&mut acc.channel, &mut acc.programmes, previous_programmes);
                    } else {
                        merge_missing_web_ui_programmes(&mut acc.channel, &mut acc.programmes, channel.programmes);
                    }
                }
                std::collections::hash_map::Entry::Vacant(entry) => {
                    let programmes = channel.programmes.iter().map(WebUiProgrammeKey::from).collect::<HashSet<_>>();
                    entry.insert(WebUiChannelAcc { priority, channel, programmes });
                }
            }
        }
    }

    let mut channels = merged.into_values().map(|entry| entry.channel).collect::<Vec<_>>();
    channels.sort_by(|left, right| left.id.cmp(&right.id));
    channels
}

async fn load_epg_channels_for_input(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
) -> Result<Option<Vec<EpgChannel>>, shared::error::TuliproxError> {
    let Some(epg_config) = input.epg.as_ref() else {
        return Ok(None);
    };

    let storage_dir = app_state.app_config.config.load().storage_dir.clone();
    let mut channels_by_source = Vec::new();
    let mut failed_sources = 0usize;

    for epg_source in &epg_config.sources {
        let resolved_url = resolve_provider_url_with_input(input, &epg_source.url);
        let raw_epg_path = match get_input_raw_epg_file_path(&resolved_url, input, &storage_dir).await {
            Ok(path) => path,
            Err(err) => {
                debug!(
                    "Skipping EPG source {}: {}",
                    sanitize_sensitive_info(resolved_url.as_str()),
                    sanitize_sensitive_info(&err.to_string())
                );
                failed_sources += 1;
                continue;
            }
        };

        let source_channels = if file_exists_async(&raw_epg_path).await {
            match parse_xmltv_for_web_ui_from_file(&raw_epg_path).await {
                Ok(ch) => ch,
                Err(file_err) => {
                    debug!(
                        "EPG file parse failed {}, trying upstream: {file_err}",
                        sanitize_sensitive_info(raw_epg_path.to_str().unwrap_or_default())
                    );
                    match parse_xmltv_for_web_ui_from_url(app_state, &resolved_url).await {
                        Ok(ch) => ch,
                        Err(url_err) => {
                            debug!(
                                "EPG url also failed {}: {}",
                                sanitize_sensitive_info(resolved_url.as_str()),
                                sanitize_sensitive_info(&url_err.to_string())
                            );
                            failed_sources += 1;
                            continue;
                        }
                    }
                }
            }
        } else {
            match parse_xmltv_for_web_ui_from_url(app_state, &resolved_url).await {
                Ok(ch) => ch,
                Err(err) => {
                    debug!(
                        "Skipping EPG url {}: {}",
                        sanitize_sensitive_info(resolved_url.as_str()),
                        sanitize_sensitive_info(&err.to_string())
                    );
                    failed_sources += 1;
                    continue;
                }
            }
        };
        channels_by_source.push((epg_source.priority, source_channels));
    }

    if channels_by_source.is_empty() {
        if failed_sources > 0 {
            Err(shared::error::TuliproxError::Config(format!(
                "All {failed_sources} EPG source(s) failed for input '{}'",
                input.name
            )))
        } else {
            Ok(None)
        }
    } else {
        Ok(Some(merge_epg_channels(channels_by_source)))
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
            let valid_targets = Arc::new(valid_targets);
            // Deduplicate rapid clicks: the channel has capacity 1, so at most one
            // update is queued at any time.  Additional requests while the channel
            // is full are silently dropped — the pending run already covers them.
            match app_state.manual_update_sender.try_send(valid_targets) {
                Ok(()) => axum::http::StatusCode::ACCEPTED.into_response(),
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    debug!("Manual playlist update deduplicated: an update is already pending or running");
                    axum::http::StatusCode::ACCEPTED.into_response()
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    debug!("Manual playlist update rejected: worker channel closed (server shutting down)");
                    axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response()
                }
            }
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

        PlaylistRequest::Input(input_name) => {
            if let Some(input) = app_state.app_config.get_input_by_name(&input_name.intern()) {
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

fn playlist_webplayer(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    target_id: u16,
    virtual_id: u32,
    cluster: XtreamCluster,
) -> impl axum::response::IntoResponse + Send {
    let access_token = create_access_token(&app_state.app_config.access_token_secret, 30);
    let config = app_state.app_config.config.load();
    let server_name = config
        .web_ui
        .as_ref()
        .and_then(|web_ui| web_ui.player_server.as_ref())
        .map_or("default", |server_name| server_name.as_str());
    let server_info = app_state.app_config.get_server_info(server_name);
    let Some(server_info) = server_info else {
        return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let base_url = server_info.get_base_url();
    build_playlist_webplayer_url(&base_url, &access_token, target_id, virtual_id, cluster).into_response()
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
        PlaylistEpgRequest::Input(input_name) => {
            if let Some(input) = app_state.app_config.get_input_by_name(&input_name.intern()) {
                match load_epg_channels_for_input(&app_state, input.as_ref()).await {
                    Ok(Some(epg)) => {
                        let config = app_state.app_config.config.load();
                        let web_ui_path =
                            config.web_ui.as_ref().and_then(|w| w.path.as_ref()).map_or("", String::as_str);
                        let resource_url = concat_path_leading_slash(web_ui_path, "api/v1/playlist/resource");
                        let encrypt_secret = app_state.get_encrypt_secret();
                        let epg = epg
                            .into_iter()
                            .map(|channel| rewrite_epg_channel_resource_url(&encrypt_secret, &resource_url, channel))
                            .collect::<Vec<_>>();
                        return json_or_bin_response(accept.as_deref(), &epg).into_response();
                    }
                    Ok(None) => return axum::http::StatusCode::NO_CONTENT.into_response(),
                    Err(err) => {
                        error!(
                            "Failed to load input EPG for '{}': {}",
                            input.name,
                            sanitize_sensitive_info(err.to_string().as_str())
                        );
                        return (
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            axum::Json(serde_json::json!({"error": "Failed to load EPG"})),
                        )
                            .into_response();
                    }
                }
            }
        }
        PlaylistEpgRequest::Custom(url) => match parse_xmltv_for_web_ui_from_url(&app_state, &url).await {
            Ok(epg) => {
                let config = app_state.app_config.config.load();
                let web_ui_path = config.web_ui.as_ref().and_then(|w| w.path.as_ref()).map_or("", String::as_str);
                let resource_url = concat_path_leading_slash(web_ui_path, "api/v1/playlist/resource");
                let encrypt_secret = app_state.get_encrypt_secret();
                let epg = epg
                    .into_iter()
                    .map(|channel| rewrite_epg_channel_resource_url(&encrypt_secret, &resource_url, channel))
                    .collect::<Vec<_>>();
                return json_or_bin_response(accept.as_deref(), &epg).into_response();
            }
            Err(err) => {
                error!("Failed to load custom EPG: {}", sanitize_sensitive_info(err.to_string().as_str()));
                return (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(serde_json::json!({"error": "Failed to load EPG"})),
                )
                    .into_response();
            }
        },
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

async fn playlist_resolve_url(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(request): axum::extract::Json<PlaylistUrlResolveRequest>,
) -> impl IntoResponse + Send {
    match request {
        PlaylistUrlResolveRequest::Webplayer { target_id, virtual_id, cluster } => {
            playlist_webplayer(axum::extract::State(app_state), target_id, virtual_id, cluster).into_response()
        }
        PlaylistUrlResolveRequest::Provider { playlist_request, url } => {
            resolve_provider_url_for_request(&app_state.app_config, &playlist_request, &url).into_response()
        }
    }
}

pub fn v1_api_playlist_register_protected(router: Router<Arc<AppState>>) -> axum::Router<Arc<AppState>> {
    router
        .route("/playlist/resolve_url", axum::routing::post(playlist_resolve_url))
        .route("/playlist/update", axum::routing::post(playlist_update))
        .route("/playlist/epg", axum::routing::post(playlist_epg))
        .route("/playlist/epg/stream", axum::routing::post(stream_epg_api))
        .route("/playlist/live", axum::routing::post(playlist_content_live))
        .route("/playlist/vod", axum::routing::post(playlist_content_vod))
        .route("/playlist/series", axum::routing::post(playlist_content_series))
        .route("/playlist/series_info/{virtual_id}/{provider_id}", axum::routing::post(playlist_series_info))
        .route("/playlist/series/episode/{virtual_id}", axum::routing::post(playlist_episode_item))
}

pub fn v1_api_playlist_register_public(router: Router<Arc<AppState>>) -> axum::Router<Arc<AppState>> {
    router.route("/playlist/resource/{resource}", axum::routing::get(playlist_resource))
}

pub fn v1_api_playlist_register_with_permissions(
    router: Router<Arc<AppState>>,
    app_state: &Arc<AppState>,
) -> axum::Router<Arc<AppState>> {
    let read_routes = Router::new()
        .route("/live", axum::routing::post(playlist_content_live))
        .route("/vod", axum::routing::post(playlist_content_vod))
        .route("/series", axum::routing::post(playlist_content_series))
        .route("/resolve_url", axum::routing::post(playlist_resolve_url))
        .route("/series_info/{virtual_id}/{provider_id}", axum::routing::post(playlist_series_info))
        .route("/series/episode/{virtual_id}", axum::routing::post(playlist_episode_item))
        .layer(permission_layer!(app_state, Permission::PlaylistRead));

    let write_routes = Router::new()
        .route("/update", axum::routing::post(playlist_update))
        .layer(permission_layer!(app_state, Permission::PlaylistWrite));

    let epg_routes = Router::new()
        .route("/epg", axum::routing::post(playlist_epg))
        .route("/epg/stream", axum::routing::post(stream_epg_api))
        .layer(permission_layer!(app_state, Permission::EpgRead));

    router.nest("/playlist", read_routes.merge(write_routes).merge(epg_routes))
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

#[cfg(test)]
mod tests {
    use super::resolve_provider_url_for_request;
    use crate::{
        api::model::{
            ActiveProviderManager, ActiveUserManager, AppState, ConnectionManager, EventManager, MetadataUpdateManager,
            PlaylistStorageState, SharedStreamManager,
        },
        model::{
            AppConfig, Config, ConfigInput, ConfigProvider, ConfigSource, ConfigTarget, SourcesConfig,
            StreamHistoryConfig,
        },
        utils::GeoIp,
        utils::epg::get_input_raw_epg_file_path,
    };
    use arc_swap::{ArcSwap, ArcSwapOption};
    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode},
    };
    use chrono::Utc;
    use shared::foundation::Filter;
    use shared::model::ProcessingOrder;
    use shared::{
        model::{
            ConfigPaths, ConfigProviderDto, EpgChannel, EpgConfigDto, EpgProgramme, EpgSourceDto, PlaylistRequest,
            XtreamCluster,
        },
        utils::Internable,
    };
    use std::sync::Arc;
    use tempfile::tempdir;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;
    use tower::ServiceExt;

    /// Generate an XMLTV datetime string in the format `YYYYMMDDHHmmss +0000`
    /// offset by `hours_from_now` hours from the current time.
    fn epg_dt(hours_from_now: i64) -> String {
        let dt = Utc::now() + chrono::Duration::hours(hours_from_now);
        dt.format("%Y%m%d%H%M%S %z").to_string()
    }

    fn test_app_config(input: Arc<ConfigInput>, source: ConfigSource) -> AppConfig {
        let sources = SourcesConfig {
            batch_files: vec![],
            provider: vec![],
            inputs: vec![input],
            sources: vec![source],
            templates: None,
        };

        AppConfig {
            config: Arc::new(ArcSwap::from_pointee(Config::default())),
            sources: Arc::new(ArcSwap::from_pointee(sources)),
            hdhomerun: Arc::new(ArcSwapOption::empty()),
            api_proxy: Arc::new(ArcSwapOption::empty()),
            file_locks: Arc::new(crate::utils::FileLockManager::default()),
            paths: Arc::new(ArcSwap::from_pointee(ConfigPaths {
                home_path: String::new(),
                config_path: String::new(),
                storage_path: String::new(),
                config_file_path: String::new(),
                sources_file_path: String::new(),
                mapping_file_path: None,
                mapping_files_used: None,
                template_file_path: None,
                template_files_used: None,
                api_proxy_file_path: String::new(),
                custom_stream_response_path: None,
            })),
            custom_stream_response: Arc::new(ArcSwapOption::empty()),
            access_token_secret: [0; 32],
            encrypt_secret: [0; 16],
            media_tools: Arc::new(crate::model::MediaToolCapabilities::default()),
        }
    }

    fn test_app_state(app_cfg: Arc<AppConfig>) -> Arc<AppState> {
        let event_manager = Arc::new(EventManager::new());
        let active_provider = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
        let shared_stream_manager = Arc::new(SharedStreamManager::new(Arc::clone(&active_provider)));
        let history_config = Some(StreamHistoryConfig::default());
        active_provider.set_shared_stream_manager(Arc::clone(&shared_stream_manager));

        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let config = app_cfg.config.load();
        let active_users = Arc::new(ActiveUserManager::new(&config, &geoip, &event_manager));
        let connection_manager = Arc::new(ConnectionManager::new(
            &active_users,
            &active_provider,
            &shared_stream_manager,
            &event_manager,
            history_config.as_ref(),
        ));

        let tokens = crate::api::model::CancelTokens {
            scheduler: CancellationToken::new(),
            hdhomerun: CancellationToken::new(),
            file_watch: CancellationToken::new(),
            provider_dns: CancellationToken::new(),
            metadata: CancellationToken::new(),
            qos_aggregation: CancellationToken::new(),
            downloads: CancellationToken::new(),
        };
        let metadata_manager = Arc::new(MetadataUpdateManager::new(tokens.metadata.clone()));
        let (manual_update_sender, _) = mpsc::channel::<Arc<crate::model::ProcessTargets>>(1);

        Arc::new(AppState {
            forced_targets: Arc::new(ArcSwap::from_pointee(crate::model::ProcessTargets {
                enabled: false,
                inputs: Vec::new(),
                targets: Vec::new(),
                target_names: Vec::new(),
            })),
            app_config: app_cfg,
            http_client: Arc::new(ArcSwap::from_pointee(reqwest::Client::new())),
            http_client_no_redirect: Arc::new(ArcSwap::from_pointee(reqwest::Client::new())),
            downloads: Arc::new(crate::api::model::DownloadQueue::new()),
            cache: Arc::new(ArcSwapOption::default()),
            shared_stream_manager,
            active_users,
            active_provider,
            connection_manager,
            event_manager,
            cancel_tokens: Arc::new(ArcSwap::from_pointee(tokens)),
            playlists: Arc::new(PlaylistStorageState::new()),
            geoip,
            update_guard: crate::api::model::UpdateGuard::new(),
            metadata_manager,
            manual_update_sender,
        })
    }

    #[test]
    fn resolve_provider_url_for_input_request_rewrites_provider_scheme() {
        let provider = ConfigProvider::from(&ConfigProviderDto {
            name: "demo".intern(),
            urls: vec!["http://provider.example".intern()],
            provider_url_selection_policy: shared::model::ProviderUrlSelectionPolicy::default(),
            dns: None,
        });
        let input = Arc::new(ConfigInput {
            id: 7,
            name: "input".intern(),
            provider_configs: Some(vec![Arc::new(provider)]),
            ..Default::default()
        });
        let source = ConfigSource { inputs: vec![Arc::clone(&input.name)], targets: vec![] };
        let app_config = test_app_config(input, source);
        let resolved = resolve_provider_url_for_request(
            &app_config,
            &PlaylistRequest::Input("input".to_string()),
            "provider://demo/live/user/pass/1359.ts",
        );

        assert_eq!(resolved, "http://provider.example/live/user/pass/1359.ts");
    }

    #[test]
    fn resolve_provider_url_for_target_request_rewrites_provider_scheme() {
        let provider = ConfigProvider::from(&ConfigProviderDto {
            name: "demo".intern(),
            urls: vec!["http://provider.example".intern()],
            provider_url_selection_policy: shared::model::ProviderUrlSelectionPolicy::default(),
            dns: None,
        });
        let input = Arc::new(ConfigInput {
            id: 7,
            name: "input".intern(),
            provider_configs: Some(vec![Arc::new(provider)]),
            ..Default::default()
        });
        let target = Arc::new(ConfigTarget {
            id: 11,
            enabled: true,
            name: "target".to_string(),
            options: None,
            sort: None,
            filter: Filter::default(),
            output: vec![],
            rename: None,
            mapping_ids: None,
            mapping: Arc::default(),
            favourites: None,
            processing_order: ProcessingOrder::default(),
            watch: None,
            use_memory_cache: false,
        });
        let source = ConfigSource { inputs: vec![Arc::clone(&input.name)], targets: vec![target] };
        let app_config = test_app_config(input, source);
        let resolved = resolve_provider_url_for_request(
            &app_config,
            &PlaylistRequest::Target(11),
            "provider://demo/live/user/pass/1359.ts",
        );

        assert_eq!(resolved, "http://provider.example/live/user/pass/1359.ts");
    }

    #[test]
    fn resolve_provider_url_passthrough_for_unresolved_provider_input_request() {
        let provider = ConfigProvider::from(&ConfigProviderDto {
            name: "demo".intern(),
            urls: vec!["http://provider.example".intern()],
            provider_url_selection_policy: shared::model::ProviderUrlSelectionPolicy::default(),
            dns: None,
        });
        let input = Arc::new(ConfigInput {
            id: 7,
            name: "input".intern(),
            provider_configs: Some(vec![Arc::new(provider)]),
            ..Default::default()
        });
        let source = ConfigSource { inputs: vec![Arc::clone(&input.name)], targets: vec![] };
        let app_config = test_app_config(input, source);
        let original = "provider://unknown/live/user/pass/1359.ts";
        let resolved =
            resolve_provider_url_for_request(&app_config, &PlaylistRequest::Input("input".to_string()), original);

        assert_eq!(resolved, original);
    }

    #[test]
    fn resolve_provider_url_passthrough_for_unresolved_provider_target_request() {
        let provider = ConfigProvider::from(&ConfigProviderDto {
            name: "demo".intern(),
            urls: vec!["http://provider.example".intern()],
            provider_url_selection_policy: shared::model::ProviderUrlSelectionPolicy::default(),
            dns: None,
        });
        let input = Arc::new(ConfigInput {
            id: 7,
            name: "input".intern(),
            provider_configs: Some(vec![Arc::new(provider)]),
            ..Default::default()
        });
        let target = Arc::new(ConfigTarget {
            id: 11,
            enabled: true,
            name: "target".to_string(),
            options: None,
            sort: None,
            filter: Filter::default(),
            output: vec![],
            rename: None,
            mapping_ids: None,
            mapping: Arc::default(),
            favourites: None,
            processing_order: ProcessingOrder::default(),
            watch: None,
            use_memory_cache: false,
        });
        let source = ConfigSource { inputs: vec![Arc::clone(&input.name)], targets: vec![target] };
        let app_config = test_app_config(input, source);
        let original = "provider://unknown/live/user/pass/1359.ts";
        let resolved = resolve_provider_url_for_request(&app_config, &PlaylistRequest::Target(11), original);

        assert_eq!(resolved, original);
    }

    #[test]
    fn resolve_provider_url_passthrough_for_ambiguous_target_request() {
        let provider_a = ConfigProvider::from(&ConfigProviderDto {
            name: "shared".intern(),
            urls: vec!["http://provider-a.example".intern()],
            provider_url_selection_policy: shared::model::ProviderUrlSelectionPolicy::default(),
            dns: None,
        });
        let provider_b = ConfigProvider::from(&ConfigProviderDto {
            name: "shared".intern(),
            urls: vec!["http://provider-b.example".intern()],
            provider_url_selection_policy: shared::model::ProviderUrlSelectionPolicy::default(),
            dns: None,
        });
        let input_a = Arc::new(ConfigInput {
            id: 7,
            name: "input-a".intern(),
            provider_configs: Some(vec![Arc::new(provider_a)]),
            ..Default::default()
        });
        let input_b = Arc::new(ConfigInput {
            id: 8,
            name: "input-b".intern(),
            provider_configs: Some(vec![Arc::new(provider_b)]),
            ..Default::default()
        });
        let target = Arc::new(ConfigTarget {
            id: 11,
            enabled: true,
            name: "target".to_string(),
            options: None,
            sort: None,
            filter: Filter::default(),
            output: vec![],
            rename: None,
            mapping_ids: None,
            mapping: Arc::default(),
            favourites: None,
            processing_order: ProcessingOrder::default(),
            watch: None,
            use_memory_cache: false,
        });
        let source =
            ConfigSource { inputs: vec![Arc::clone(&input_a.name), Arc::clone(&input_b.name)], targets: vec![target] };
        let sources = SourcesConfig {
            batch_files: vec![],
            provider: vec![],
            inputs: vec![input_a, input_b],
            sources: vec![source],
            templates: None,
        };

        let app_config = AppConfig {
            config: Arc::new(ArcSwap::from_pointee(Config::default())),
            sources: Arc::new(ArcSwap::from_pointee(sources)),
            hdhomerun: Arc::new(ArcSwapOption::empty()),
            api_proxy: Arc::new(ArcSwapOption::empty()),
            file_locks: Arc::new(crate::utils::FileLockManager::default()),
            paths: Arc::new(ArcSwap::from_pointee(ConfigPaths {
                home_path: String::new(),
                config_path: String::new(),
                storage_path: String::new(),
                config_file_path: String::new(),
                sources_file_path: String::new(),
                mapping_file_path: None,
                mapping_files_used: None,
                template_file_path: None,
                template_files_used: None,
                api_proxy_file_path: String::new(),
                custom_stream_response_path: None,
            })),
            custom_stream_response: Arc::new(ArcSwapOption::empty()),
            access_token_secret: [0; 32],
            encrypt_secret: [0; 16],
            media_tools: Arc::new(crate::model::MediaToolCapabilities::default()),
        };

        let original = "provider://shared/live/user/pass/1359.ts";
        let resolved = resolve_provider_url_for_request(&app_config, &PlaylistRequest::Target(11), original);

        assert_eq!(resolved, original);
    }

    #[test]
    fn build_playlist_webplayer_url_uses_cluster_stream_type() {
        let live = super::build_playlist_webplayer_url("http://player.example", "token123", 1, 42, XtreamCluster::Live);
        let movie =
            super::build_playlist_webplayer_url("http://player.example", "token123", 1, 42, XtreamCluster::Video);
        let series =
            super::build_playlist_webplayer_url("http://player.example", "token123", 1, 42, XtreamCluster::Series);

        assert_eq!(live, "http://player.example/token/token123/1/live/42");
        assert_eq!(movie, "http://player.example/token/token123/1/movie/42");
        assert_eq!(series, "http://player.example/token/token123/1/series/42");
    }

    #[test]
    fn merge_epg_channels_prefers_higher_priority_metadata_and_merges_all_programmes() {
        let low_priority = EpgChannel {
            id: "demo.channel".intern(),
            title: Some("Low".intern()),
            icon: Some("http://low/icon.png".intern()),
            programmes: vec![EpgProgramme::new_all(10, 20, "demo.channel".intern(), Some("Low Show".intern()), None)],
        };
        let high_priority = EpgChannel {
            id: "demo.channel".intern(),
            title: Some("High".intern()),
            icon: Some("http://high/icon.png".intern()),
            programmes: vec![EpgProgramme::new_all(30, 40, "demo.channel".intern(), Some("High Show".intern()), None)],
        };
        let same_priority = EpgChannel {
            id: "demo.channel".intern(),
            title: Some("Same".intern()),
            icon: Some("http://same/icon.png".intern()),
            programmes: vec![
                EpgProgramme::new_all(30, 40, "demo.channel".intern(), Some("Duplicate".intern()), None),
                EpgProgramme::new_all(50, 60, "demo.channel".intern(), Some("Second Show".intern()), None),
            ],
        };

        let channels = super::merge_epg_channels(vec![
            (10, vec![low_priority]),
            (0, vec![high_priority]),
            (0, vec![same_priority]),
        ]);

        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0].title.as_deref(), Some("High"));
        assert_eq!(channels[0].icon.as_deref(), Some("http://high/icon.png"));
        assert_eq!(channels[0].programmes.len(), 3);
        assert_eq!(
            channels[0].programmes.iter().map(|programme| (programme.start, programme.stop)).collect::<Vec<_>>(),
            vec![(10, 20), (30, 40), (50, 60)],
        );
    }

    #[tokio::test]
    async fn load_epg_channels_for_input_uses_resolved_provider_epg_cache_file() {
        let temp_dir = tempdir().expect("temp dir");
        let provider = ConfigProvider::from(&ConfigProviderDto {
            name: "demo".intern(),
            urls: vec!["http://provider.example".intern()],
            provider_url_selection_policy: shared::model::ProviderUrlSelectionPolicy::default(),
            dns: None,
        });
        let input = Arc::new(ConfigInput {
            id: 7,
            name: "input".intern(),
            epg: Some(crate::model::EpgConfig::from(&EpgConfigDto {
                sources: Some(vec![EpgSourceDto {
                    url: "provider://demo/xmltv.php?username=user&password=pass".to_string(),
                    priority: 0,
                    logo_override: false,
                }]),
                t_sources: vec![EpgSourceDto {
                    url: "provider://demo/xmltv.php?username=user&password=pass".to_string(),
                    priority: 0,
                    logo_override: false,
                }],
                smart_match: None,
            })),
            provider_configs: Some(vec![Arc::new(provider)]),
            ..Default::default()
        });
        let source = ConfigSource { inputs: vec![Arc::clone(&input.name)], targets: vec![] };
        let app_config = test_app_config(input.clone(), source);
        app_config.config.store(Arc::new(Config {
            storage_dir: temp_dir.path().to_string_lossy().to_string(),
            ..Default::default()
        }));
        let raw_epg_path = get_input_raw_epg_file_path(
            "http://provider.example/xmltv.php?username=user&password=pass",
            input.as_ref(),
            temp_dir.path().to_string_lossy().as_ref(),
        )
        .await
        .expect("epg path");
        if let Some(parent) = raw_epg_path.parent() {
            tokio::fs::create_dir_all(parent).await.expect("epg dir");
        }
        let prog_start = epg_dt(0);
        let prog_stop = epg_dt(1);
        tokio::fs::write(
            &raw_epg_path,
            format!(
                r#"<?xml version="1.0" encoding="utf-8"?>
<tv>
  <channel id="demo.channel">
    <display-name>Demo Channel</display-name>
  </channel>
  <programme start="{prog_start}" stop="{prog_stop}" channel="demo.channel">
    <title>Morning Show</title>
  </programme>
</tv>"#
            ),
        )
        .await
        .expect("write epg");

        let app_state = test_app_state(Arc::new(app_config));
        let channels =
            super::load_epg_channels_for_input(&app_state, input.as_ref()).await.expect("load epg").expect("channels");

        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0].id.as_ref(), "demo.channel");
        assert_eq!(channels[0].programmes.len(), 1);
        assert_eq!(channels[0].programmes[0].title.as_ref().map(std::convert::AsRef::as_ref), Some("Morning Show"));
    }

    #[tokio::test]
    async fn playlist_epg_input_route_returns_cached_provider_epg() {
        let temp_dir = tempdir().expect("temp dir");
        let provider = ConfigProvider::from(&ConfigProviderDto {
            name: "demo".intern(),
            urls: vec!["http://provider.example".intern()],
            provider_url_selection_policy: shared::model::ProviderUrlSelectionPolicy::default(),
            dns: None,
        });
        let input = Arc::new(ConfigInput {
            id: 7,
            name: "input".intern(),
            epg: Some(crate::model::EpgConfig::from(&EpgConfigDto {
                sources: Some(vec![EpgSourceDto {
                    url: "provider://demo/xmltv.php?username=user&password=pass".to_string(),
                    priority: 0,
                    logo_override: false,
                }]),
                t_sources: vec![EpgSourceDto {
                    url: "provider://demo/xmltv.php?username=user&password=pass".to_string(),
                    priority: 0,
                    logo_override: false,
                }],
                smart_match: None,
            })),
            provider_configs: Some(vec![Arc::new(provider)]),
            ..Default::default()
        });
        let source = ConfigSource { inputs: vec![Arc::clone(&input.name)], targets: vec![] };
        let app_config = test_app_config(input.clone(), source);
        app_config.config.store(Arc::new(Config {
            storage_dir: temp_dir.path().to_string_lossy().to_string(),
            ..Default::default()
        }));
        let raw_epg_path = get_input_raw_epg_file_path(
            "http://provider.example/xmltv.php?username=user&password=pass",
            input.as_ref(),
            temp_dir.path().to_string_lossy().as_ref(),
        )
        .await
        .expect("epg path");
        if let Some(parent) = raw_epg_path.parent() {
            tokio::fs::create_dir_all(parent).await.expect("epg dir");
        }
        let prog_start = epg_dt(0);
        let prog_stop = epg_dt(1);
        tokio::fs::write(
            &raw_epg_path,
            format!(
                r#"<?xml version="1.0" encoding="utf-8"?>
<tv>
  <channel id="demo.channel">
    <display-name>Demo Channel</display-name>
  </channel>
  <programme start="{prog_start}" stop="{prog_stop}" channel="demo.channel">
    <title>Morning Show</title>
  </programme>
</tv>"#
            ),
        )
        .await
        .expect("write epg");

        let app_state = test_app_state(Arc::new(app_config));
        let router = super::v1_api_playlist_register_protected(Router::new()).with_state(app_state);
        let request = Request::builder()
            .method("POST")
            .uri("/playlist/epg")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"Input":"input"}"#))
            .expect("request");

        let response = router.into_service::<Body>().oneshot(request).await.expect("response");

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("body");
        let body_text = String::from_utf8(body.to_vec()).expect("utf8");
        assert!(body_text.contains("demo.channel"), "{body_text}");
        assert!(body_text.contains("Morning Show"), "{body_text}");
    }

    #[tokio::test]
    async fn playlist_epg_input_route_returns_no_content_for_unknown_input_name() {
        let provider = ConfigProvider::from(&ConfigProviderDto {
            name: "demo".intern(),
            urls: vec!["http://provider.example".intern()],
            provider_url_selection_policy: shared::model::ProviderUrlSelectionPolicy::default(),
            dns: None,
        });
        let input = Arc::new(ConfigInput {
            id: 7,
            name: "input".intern(),
            provider_configs: Some(vec![Arc::new(provider)]),
            ..Default::default()
        });
        let source = ConfigSource { inputs: vec![Arc::clone(&input.name)], targets: vec![] };
        let app_state = test_app_state(Arc::new(test_app_config(input, source)));
        let router = super::v1_api_playlist_register_protected(Router::new()).with_state(app_state);
        let request = Request::builder()
            .method("POST")
            .uri("/playlist/epg")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"Input":"missing"}"#))
            .expect("request");

        let response = router.into_service::<Body>().oneshot(request).await.expect("response");

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn playlist_epg_input_route_merges_multiple_cached_sources_by_priority() {
        let temp_dir = tempdir().expect("temp dir");
        let provider = ConfigProvider::from(&ConfigProviderDto {
            name: "demo".intern(),
            urls: vec!["http://provider.example".intern()],
            provider_url_selection_policy: shared::model::ProviderUrlSelectionPolicy::default(),
            dns: None,
        });
        let primary_url = "provider://demo/xmltv-primary.php?username=user&password=pass";
        let secondary_url = "provider://demo/xmltv-secondary.php?username=user&password=pass";
        let input = Arc::new(ConfigInput {
            id: 7,
            name: "input".intern(),
            epg: Some(crate::model::EpgConfig::from(&EpgConfigDto {
                sources: Some(vec![
                    EpgSourceDto { url: secondary_url.to_string(), priority: 10, logo_override: false },
                    EpgSourceDto { url: primary_url.to_string(), priority: 0, logo_override: false },
                    EpgSourceDto {
                        url: "provider://demo/xmltv-same-priority.php?username=user&password=pass".to_string(),
                        priority: 0,
                        logo_override: false,
                    },
                ]),
                t_sources: vec![
                    EpgSourceDto { url: secondary_url.to_string(), priority: 10, logo_override: false },
                    EpgSourceDto { url: primary_url.to_string(), priority: 0, logo_override: false },
                    EpgSourceDto {
                        url: "provider://demo/xmltv-same-priority.php?username=user&password=pass".to_string(),
                        priority: 0,
                        logo_override: false,
                    },
                ],
                smart_match: None,
            })),
            provider_configs: Some(vec![Arc::new(provider)]),
            ..Default::default()
        });
        let source = ConfigSource { inputs: vec![Arc::clone(&input.name)], targets: vec![] };
        let app_config = test_app_config(input.clone(), source);
        app_config.config.store(Arc::new(Config {
            storage_dir: temp_dir.path().to_string_lossy().to_string(),
            ..Default::default()
        }));

        let primary_path = get_input_raw_epg_file_path(
            "http://provider.example/xmltv-primary.php?username=user&password=pass",
            input.as_ref(),
            temp_dir.path().to_string_lossy().as_ref(),
        )
        .await
        .expect("primary epg path");
        let secondary_path = get_input_raw_epg_file_path(
            "http://provider.example/xmltv-secondary.php?username=user&password=pass",
            input.as_ref(),
            temp_dir.path().to_string_lossy().as_ref(),
        )
        .await
        .expect("secondary epg path");
        let same_priority_path = get_input_raw_epg_file_path(
            "http://provider.example/xmltv-same-priority.php?username=user&password=pass",
            input.as_ref(),
            temp_dir.path().to_string_lossy().as_ref(),
        )
        .await
        .expect("same priority epg path");

        for path in [&primary_path, &secondary_path, &same_priority_path] {
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await.expect("epg dir");
            }
        }

        let p0 = epg_dt(0);
        let p1 = epg_dt(1);
        let p2 = epg_dt(2);
        let p3 = epg_dt(3);

        tokio::fs::write(
            &secondary_path,
            format!(
                r#"<?xml version="1.0" encoding="utf-8"?>
<tv>
  <channel id="demo.channel">
    <display-name>Secondary Channel</display-name>
    <icon src="http://secondary/icon.png" />
  </channel>
  <programme start="{p0}" stop="{p1}" channel="demo.channel">
    <title>Secondary Show</title>
  </programme>
</tv>"#
            ),
        )
        .await
        .expect("write secondary epg");
        tokio::fs::write(
            &primary_path,
            format!(
                r#"<?xml version="1.0" encoding="utf-8"?>
<tv>
  <channel id="demo.channel">
    <display-name>Primary Channel</display-name>
    <icon src="http://primary/icon.png" />
  </channel>
  <programme start="{p1}" stop="{p2}" channel="demo.channel">
    <title>Primary Show</title>
  </programme>
</tv>"#
            ),
        )
        .await
        .expect("write primary epg");
        tokio::fs::write(
            &same_priority_path,
            format!(
                r#"<?xml version="1.0" encoding="utf-8"?>
<tv>
  <channel id="demo.channel">
    <display-name>Same Priority Channel</display-name>
    <icon src="http://same/icon.png" />
  </channel>
  <programme start="{p0}" stop="{p1}" channel="demo.channel">
    <title>Duplicate Show</title>
  </programme>
  <programme start="{p2}" stop="{p3}" channel="demo.channel">
    <title>Second Show</title>
  </programme>
</tv>"#
            ),
        )
        .await
        .expect("write same priority epg");

        let app_state = test_app_state(Arc::new(app_config));
        let router = super::v1_api_playlist_register_protected(Router::new()).with_state(app_state);
        let request = Request::builder()
            .method("POST")
            .uri("/playlist/epg")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"Input":"input"}"#))
            .expect("request");

        let response = router.into_service::<Body>().oneshot(request).await.expect("response");

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("body");
        let body_text = String::from_utf8(body.to_vec()).expect("utf8");
        assert!(body_text.contains("Primary Channel"), "{body_text}");
        assert!(!body_text.contains("Secondary Channel"), "{body_text}");
        assert!(body_text.contains("Primary Show"), "{body_text}");
        assert!(body_text.contains("Second Show"), "{body_text}");
        assert!(!body_text.contains("Secondary Show"), "{body_text}");
    }

}
