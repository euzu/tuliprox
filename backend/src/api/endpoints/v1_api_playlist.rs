use crate::{
    api::{
        api_utils::{
            create_api_proxy_user, json_or_bin_response, resource_response, try_option_bad_request,
            try_result_bad_request, try_unwrap_body,
        },
        endpoints::{
            api_playlist_utils::{
                get_playlist_for_custom_provider, get_playlist_for_input, get_playlist_for_target,
                STALKER_RESOURCE_SCHEME,
            },
            extract_accept_header::ExtractAcceptHeader,
            m3u_api::m3u_api_stream_loaded,
            xmltv_api::{rewrite_epg_channel_resource_url, serve_epg_web_ui, stream_epg_api},
            xtream_api::{
                xtream_get_stream_info_response, xtream_player_api_stream_with_resolved_target,
                xtream_player_api_stream_with_token, ApiStreamContext, ApiStreamRequest,
            },
        },
        model::AppState,
    },
    auth::{create_access_token, permission_layer, verify_access_token},
    iptv::{stalker::client::validate_public_playable_url, xtream},
    model::{
        parse_xmltv_for_web_ui_from_file, parse_xmltv_for_web_ui_from_url, AppConfig, ConfigInput, ConfigInputFlags,
        ConfigInputOptions, EpgSource, EpgSourceType, IcsDummyPolicy, InputSource,
    },
    processing::{
        parser::{
        ics::parse_ics_file_to_channel,
        xmltv::{merge_epg_channels_by_priority_with_dummy_policies, EpgDummyPolicySource},
        },
        processor::re_resolve_stalker_url,
    },
    repository::{
        iter_raw_m3u_target_playlist, iter_raw_xtream_target_playlist, m3u_get_item_for_stream_id,
        xtream_get_item_for_stream_id,
    },
    utils::{
        epg::get_input_raw_epg_file_path,
        file_exists_async,
        request,
    },
};
use axum::{response::IntoResponse, Router};
use log::{debug, error};
use serde::Deserialize;
use serde_json::json;
use shared::{
    error::TuliproxError,
    foundation::{get_filter_detailed, Filter, ValueProvider},
    model::{
        permission::Permission, stalker::StalkerStreamKind, EpgChannel, InputType, OperationRunAccepted,
        PlaylistEpgRequest, PlaylistItem, PlaylistRequest, PlaylistUrlResolveRequest, ProxyType, TargetType,
        UiPlaylistItem, XtreamCluster,
    },
    utils::{concat_path_leading_slash, deobfuscate_text, sanitize_sensitive_info, Internable},
};
use std::{path::Path, str::FromStr, sync::Arc};
use tokio_stream::StreamExt;
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
            resolve_delay: shared::defaults::default_resolve_delay_secs(),
            probe_delay: shared::defaults::default_probe_delay_secs(),
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
            resolve_delay: shared::defaults::default_resolve_delay_secs(),
            probe_delay: shared::defaults::default_probe_delay_secs(),
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
    format!(
        "{base_url}/api/v1/playlist/webplayer/{access_token}/{}/{}/{}",
        target_id,
        cluster.as_stream_type(),
        virtual_id
    )
}

fn build_recording_stream_url(
    base_url: &str,
    access_token: &str,
    target_name: &str,
    input_name: &str,
    virtual_id: u32,
    cluster: XtreamCluster,
) -> Option<String> {
    let mut url = Url::parse(base_url).ok()?;
    url.path_segments_mut().ok()?.pop_if_empty().extend([
        "api",
        "v1",
        "playlist",
        "recording",
        access_token,
        cluster.as_stream_type(),
        &virtual_id.to_string(),
    ]);
    url.query_pairs_mut().append_pair("target_name", target_name).append_pair("input_name", input_name);
    Some(url.into())
}

pub(in crate::api) fn build_recording_source_descriptor(
    target_name: &str,
    input_name: &str,
    virtual_id: u32,
    cluster: XtreamCluster,
) -> Option<String> {
    let mut url = Url::parse("tuliprox-recording://source").ok()?;
    url.query_pairs_mut()
        .append_pair("target_name", target_name)
        .append_pair("input_name", input_name)
        .append_pair("virtual_id", &virtual_id.to_string())
        .append_pair("cluster", cluster.as_stream_type());
    Some(url.into())
}

pub(in crate::api) fn build_webplayer_recording_url(
    app_config: &crate::model::AppConfig,
    target_id: u16,
    virtual_id: u32,
    cluster: XtreamCluster,
) -> Option<String> {
    let access_token = create_access_token(&app_config.access_token_secret, 30);
    let config = app_config.config.load();
    let server_name = config
        .web_ui
        .as_ref()
        .and_then(|web_ui| web_ui.player_server.as_ref())
        .map_or("default", |server_name| server_name.as_str());
    let server_info = app_config.get_server_info(server_name)?;
    Some(build_playlist_webplayer_url(
        &server_info.get_base_url(),
        &access_token,
        target_id,
        virtual_id,
        cluster,
    ))
}

pub(in crate::api) fn build_stable_recording_url(
    app_config: &crate::model::AppConfig,
    target_name: &str,
    input_name: &str,
    virtual_id: u32,
    cluster: XtreamCluster,
) -> Option<String> {
    let access_token = create_access_token(&app_config.access_token_secret, 30);
    let config = app_config.config.load();
    let server_name = config
        .web_ui
        .as_ref()
        .and_then(|web_ui| web_ui.player_server.as_ref())
        .map_or("default", |server_name| server_name.as_str());
    let server_info = app_config.get_server_info(server_name)?;
    build_recording_stream_url(
        &server_info.get_base_url(),
        &access_token,
        target_name,
        input_name,
        virtual_id,
        cluster,
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::api) struct ResolvedRecordingSource {
    pub virtual_id: u32,
    pub input_name: String,
}

#[derive(Debug, Clone)]
pub(in crate::api) struct ResolvedRecordingConfig {
    pub target: Arc<crate::model::ConfigTarget>,
    pub input: Arc<ConfigInput>,
}

pub(in crate::api) fn resolve_recording_config(
    sources: &crate::model::SourcesConfig,
    target_name: &str,
    input_name: &str,
) -> Option<ResolvedRecordingConfig> {
    let source = sources.sources.iter().find(|source| {
        source.inputs.iter().any(|configured_input| configured_input.as_ref() == input_name)
            && source.targets.iter().any(|target| target.name == target_name)
    })?;
    let target = source.targets.iter().find(|target| target.name == target_name)?.clone();
    let input = sources.inputs.iter().find(|input| input.name.as_ref() == input_name)?.clone();
    Some(ResolvedRecordingConfig { target, input })
}

pub(in crate::api) fn resolve_recording_target(
    app_config: &crate::model::AppConfig,
    target_name: &str,
    input_name: &str,
) -> Option<Arc<crate::model::ConfigTarget>> {
    resolve_recording_config(app_config.sources.load().as_ref(), target_name, input_name)
        .map(|resolved| resolved.target)
}

pub(in crate::api) async fn resolve_target_recording_source(
    app_config: &crate::model::AppConfig,
    target_name: &str,
    input_name: &str,
    virtual_id: u32,
    cluster: XtreamCluster,
) -> Option<ResolvedRecordingSource> {
    let target = resolve_recording_target(app_config, target_name, input_name)?;
    let mut resolved = None;
    if target.has_output(TargetType::Xtream) {
        if let Some(mut items) = iter_raw_xtream_target_playlist(app_config, &target, cluster).await {
            while let Some(entry) = items.next().await {
                let Ok(item) = entry else { continue };
                if item.virtual_id == virtual_id {
                    resolved = Some(ResolvedRecordingSource {
                        virtual_id: item.virtual_id,
                        input_name: item.input_name.to_string(),
                    });
                    break;
                }
            }
        }
    }
    if resolved.is_none() && target.has_output(TargetType::M3u) {
        if let Some(mut items) = iter_raw_m3u_target_playlist(app_config, &target, Some(cluster)).await {
            while let Some(entry) = items.next().await {
                let Ok(item) = entry else { continue };
                if item.virtual_id == virtual_id {
                    resolved = Some(ResolvedRecordingSource {
                        virtual_id: item.virtual_id,
                        input_name: item.input_name.to_string(),
                    });
                    break;
                }
            }
        }
    }
    let resolved = resolved?;
    (resolved.input_name == input_name).then_some(resolved)
}

pub(in crate::api) async fn resolve_target_live_recording_source_by_epg_channel(
    app_config: &crate::model::AppConfig,
    target_name: &str,
    epg_channel_id: &str,
) -> Option<ResolvedRecordingSource> {
    let targets = app_config
        .sources
        .load()
        .sources
        .iter()
        .flat_map(|source| source.targets.iter())
        .filter(|target| target.name == target_name)
        .cloned()
        .collect::<Vec<_>>();
    let mut resolved = None;
    for target in targets {
        if target.has_output(TargetType::Xtream) {
            let mut items = iter_raw_xtream_target_playlist(app_config, &target, XtreamCluster::Live).await?;
            while let Some(entry) = items.next().await {
                let Ok(item) = entry else { continue };
                if item.epg_channel_id.as_deref() == Some(epg_channel_id) {
                    let candidate = ResolvedRecordingSource {
                        virtual_id: item.virtual_id,
                        input_name: item.input_name.to_string(),
                    };
                    if resolved.replace(candidate).is_some() {
                        return None;
                    }
                }
            }
        } else if target.has_output(TargetType::M3u) {
            let mut items = iter_raw_m3u_target_playlist(app_config, &target, Some(XtreamCluster::Live)).await?;
            while let Some(entry) = items.next().await {
                let Ok(item) = entry else { continue };
                if item.epg_channel_id.as_deref() == Some(epg_channel_id) {
                    let candidate = ResolvedRecordingSource {
                        virtual_id: item.virtual_id,
                        input_name: item.input_name.to_string(),
                    };
                    if resolved.replace(candidate).is_some() {
                        return None;
                    }
                }
            }
        }
    }
    resolved
}

#[cfg(test)]
fn merge_epg_channels(mut channels_by_source: Vec<(i16, Vec<EpgChannel>)>) -> Vec<EpgChannel> {
    channels_by_source.sort_by_key(|(priority, _)| *priority);
    merge_epg_channels_by_priority_with_dummy_policies(channels_by_source, Vec::new())
}

async fn load_xmltv_epg_source_channels(
    app_state: &Arc<AppState>,
    raw_epg_path: &Path,
    resolved_url: &str,
) -> Result<Vec<EpgChannel>, TuliproxError> {
    {
        let _cache_lock = app_state.app_config.file_locks.read_lock(raw_epg_path).await;
        if file_exists_async(raw_epg_path).await {
            match parse_xmltv_for_web_ui_from_file(raw_epg_path).await {
                Ok(channels) => return Ok(channels),
                Err(file_err) => {
                    debug!(
                        "EPG file parse failed {}, trying upstream: {file_err}",
                        sanitize_sensitive_info(raw_epg_path.to_str().unwrap_or_default())
                    );
                }
            }
        }
    }

    parse_xmltv_for_web_ui_from_url(app_state, resolved_url).await
}

async fn load_ics_epg_source_channels(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    epg_source: &EpgSource,
    raw_epg_path: &Path,
    storage_dir: &str,
) -> Result<Vec<EpgChannel>, TuliproxError> {
    let ics_config = epg_source
        .ics
        .as_ref()
        .ok_or_else(|| TuliproxError::ConfigEpg("ics configuration is required for ICS EPG sources".to_string()))?;

    let channel_id = epg_source
        .channel_id
        .clone()
        .ok_or_else(|| TuliproxError::ConfigEpg("channel_id is required for ICS EPG sources".to_string()))?;

    {
        let _cache_lock = app_state.app_config.file_locks.read_lock(raw_epg_path).await;
        if file_exists_async(raw_epg_path).await {
            match parse_ics_file_to_channel(
                raw_epg_path,
                channel_id.clone(),
                epg_source.channel_title.clone(),
                ics_config,
            )
            .await
            {
                Ok(channel) => return Ok(vec![channel]),
                Err(file_err) => {
                    debug!(
                        "ICS EPG file parse failed {}, redownloading source: {}",
                        sanitize_sensitive_info(raw_epg_path.to_str().unwrap_or_default()),
                        sanitize_sensitive_info(&file_err.to_string())
                    );
                }
            }
        }
    }

    let client = app_state.http_client.load();
    request::get_input_epg_content_as_file(
        &app_state.app_config,
        &client,
        input,
        request::InputEpgFileRequest {
            headers: None,
            storage_dir,
            url: &epg_source.url,
            persist_path: raw_epg_path,
            max_bytes: Some(ics_config.max_download_bytes),
        },
    )
    .await?;

    let _cache_lock = app_state.app_config.file_locks.read_lock(raw_epg_path).await;
    parse_ics_file_to_channel(raw_epg_path, channel_id, epg_source.channel_title.clone(), ics_config)
        .await
        .map(|channel| vec![channel])
}

async fn load_epg_channels_for_input(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
) -> Result<Option<Vec<EpgChannel>>, TuliproxError> {
    let Some(epg_config) = input.epg.as_ref() else {
        return Ok(None);
    };

    let storage_dir = app_state.app_config.config.load().storage_dir.clone();
    let mut channels_by_source = Vec::new();
    let mut dummy_policies = Vec::new();
    let mut failed_sources = 0usize;

    for (source_order, epg_source) in epg_config.sources.iter().enumerate() {
        let raw_epg_path = match get_input_raw_epg_file_path(epg_source, input, &storage_dir).await {
            Ok(path) => path,
            Err(err) => {
                debug!(
                    "Skipping EPG source {}: {}",
                    sanitize_sensitive_info(epg_source.url.as_str()),
                    sanitize_sensitive_info(&err.to_string())
                );
                failed_sources += 1;
                continue;
            }
        };

        let source_result = match epg_source.source_type {
            EpgSourceType::Xmltv => {
                let resolved_url = resolve_provider_url_with_input(input, &epg_source.url);
                load_xmltv_epg_source_channels(app_state, &raw_epg_path, &resolved_url).await
            }
            EpgSourceType::Ics => {
                load_ics_epg_source_channels(app_state, input, epg_source, &raw_epg_path, &storage_dir).await
            }
        };

        let source_channels = match source_result {
            Ok(channels) => channels,
            Err(err) => {
                debug!(
                    "Skipping EPG source {}: {}",
                    sanitize_sensitive_info(epg_source.url.as_str()),
                    sanitize_sensitive_info(&err.to_string())
                );
                failed_sources += 1;
                continue;
            }
        };
        if epg_source.source_type == EpgSourceType::Ics {
            if let (Some(ics_config), Some(channel_id)) = (epg_source.ics.as_ref(), epg_source.channel_id.as_ref()) {
                dummy_policies.push(EpgDummyPolicySource {
                    priority: epg_source.priority,
                    source_order,
                    channel_id: channel_id.clone(),
                    policy: IcsDummyPolicy { timezone: ics_config.timezone.clone(), config: ics_config.dummy.clone() },
                });
            }
        }
        channels_by_source.push((epg_source.priority, source_channels));
    }

    if channels_by_source.is_empty() {
        if failed_sources > 0 {
            Err(TuliproxError::Config(format!("All {failed_sources} EPG source(s) failed for input '{}'", input.name)))
        } else {
            Ok(None)
        }
    } else {
        channels_by_source.sort_by_key(|(priority, _)| *priority);
        Ok(Some(merge_epg_channels_by_priority_with_dummy_policies(channels_by_source, dummy_policies)))
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
            match app_state
                .manual_update_sender
                .try_send(crate::api::model::ManualPlaylistUpdateRequest { targets: valid_targets })
            {
                Ok(()) => (axum::http::StatusCode::ACCEPTED, axum::Json(OperationRunAccepted {})).into_response(),
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    debug!("Manual playlist update deduplicated: an update is already pending or running");
                    (axum::http::StatusCode::ACCEPTED, axum::Json(OperationRunAccepted {})).into_response()
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    debug!("Manual playlist update rejected: worker channel closed (server shutting down)");
                    axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response()
                }
            }
        }
        Err(err) => {
            error!("Failed playlist update {}", sanitize_sensitive_info(&err.to_string()));
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
                                &app_state.http_client.load(),
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
                        &app_state.http_client.load(),
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

fn playlist_webplayer(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    target_id: u16,
    virtual_id: u32,
    cluster: XtreamCluster,
) -> impl axum::response::IntoResponse + Send {
    let Some(url) = build_webplayer_recording_url(&app_state.app_config, target_id, virtual_id, cluster) else {
        return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    url.into_response()
}

async fn playlist_webplayer_stream(
    fingerprint: crate::auth::Fingerprint,
    axum::extract::Path((token, target_id, cluster, stream_id)): axum::extract::Path<(String, u16, String, String)>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    req_headers: axum::http::HeaderMap,
) -> impl IntoResponse + Send {
    if !verify_access_token(&token, &app_state.app_config.access_token_secret) {
        return axum::http::StatusCode::FORBIDDEN.into_response();
    }

    let ctxt = try_result_bad_request!(ApiStreamContext::from_str(cluster.as_str()));
    let Some(target) = app_state.app_config.get_target_by_id(target_id) else {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    };

    if target.has_output(TargetType::Xtream) {
        return xtream_player_api_stream_with_token(
            &fingerprint,
            &req_headers,
            &app_state,
            target_id,
            ApiStreamRequest::from_access_token(ctxt, &token, &stream_id, ""),
        )
        .await
        .into_response();
    }

    if !target.has_output(TargetType::M3u) {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    }

    let req_virtual_id: u32 = try_result_bad_request!(stream_id.trim().parse());
    let pli = try_result_bad_request!(
        m3u_get_item_for_stream_id(req_virtual_id, &app_state, &target).await,
        true,
        format!("Failed to read m3u item for stream id {req_virtual_id}")
    );
    let input = try_option_bad_request!(
        app_state.app_config.get_input_by_name(&pli.input_name),
        true,
        format!("Can't find input {} for target {}", pli.input_name, target.name)
    );
    let user = Arc::new(create_api_proxy_user(&app_state));

    m3u_api_stream_loaded(user, target, &fingerprint, &req_headers, &app_state, pli, input, None, None)
        .await
        .into_response()
}

#[derive(Debug, Deserialize)]
struct RecordingStreamQuery {
    target_name: String,
    input_name: String,
}

async fn playlist_recording_stream(
    fingerprint: crate::auth::Fingerprint,
    axum::extract::Path((token, cluster, virtual_id)): axum::extract::Path<(String, String, u32)>,
    axum::extract::Query(query): axum::extract::Query<RecordingStreamQuery>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    req_headers: axum::http::HeaderMap,
) -> impl IntoResponse + Send {
    if !verify_access_token(&token, &app_state.app_config.access_token_secret) {
        return axum::http::StatusCode::FORBIDDEN.into_response();
    }
    let ctxt = try_result_bad_request!(ApiStreamContext::from_str(cluster.as_str()));
    let resolved = {
        let sources = app_state.app_config.sources.load();
        resolve_recording_config(sources.as_ref(), &query.target_name, &query.input_name)
    };
    let Some(resolved) = resolved else {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    };

    if resolved.target.has_output(TargetType::Xtream) {
        let stream_id = virtual_id.to_string();
        return xtream_player_api_stream_with_resolved_target(
            &fingerprint,
            &req_headers,
            &app_state,
            resolved.target,
            Some(resolved.input),
            ApiStreamRequest::from_access_token(ctxt, &token, &stream_id, ""),
        )
        .await
        .into_response();
    }
    if !resolved.target.has_output(TargetType::M3u) {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    }

    let pli = try_result_bad_request!(
        m3u_get_item_for_stream_id(virtual_id, &app_state, &resolved.target).await,
        true,
        format!("Failed to read m3u item for stream id {virtual_id}")
    );
    if pli.input_name != resolved.input.name || XtreamCluster::try_from(pli.item_type).ok() != Some(ctxt.cluster()) {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    }
    let user = Arc::new(create_api_proxy_user(&app_state));
    m3u_api_stream_loaded(
        user,
        resolved.target,
        &fingerprint,
        &req_headers,
        &app_state,
        pli,
        resolved.input,
        None,
        None,
    )
    .await
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
                let epg_path = crate::api::endpoints::xmltv_api::get_epg_path_for_target_by_type(
                    config,
                    &target,
                    TargetType::Xtream,
                )
                .or_else(|| {
                    crate::api::endpoints::xmltv_api::get_epg_path_for_target_by_type(config, &target, TargetType::M3u)
                });
                if let Some(epg_path) = epg_path {
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
                            sanitize_sensitive_info(&err.to_string())
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
        PlaylistEpgRequest::Custom(url) => {
            let valid_custom_url = Url::parse(&url).is_ok_and(|parsed| matches!(parsed.scheme(), "http" | "https"));
            if !valid_custom_url {
                return (
                    axum::http::StatusCode::BAD_REQUEST,
                    axum::Json(serde_json::json!({"error": "Invalid EPG URL"})),
                )
                    .into_response();
            }
            match parse_xmltv_for_web_ui_from_url(&app_state, &url).await {
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
                    error!("Failed to load custom EPG: {}", sanitize_sensitive_info(&err.to_string()));
                    return (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        axum::Json(serde_json::json!({"error": "Failed to load EPG"})),
                    )
                        .into_response();
                }
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
        if let Some((input_id, cluster, provider_id)) = parse_stalker_resource(&resource_url) {
            return stalker_resource_response(&app_state, input_id, cluster, provider_id).await;
        }
        resource_response(&app_state, &resource_url, &req_headers, None).await.into_response()
    } else {
        axum::http::StatusCode::BAD_REQUEST.into_response()
    }
}

fn parse_stalker_resource(resource: &str) -> Option<(u16, XtreamCluster, u32)> {
    let mut parts = resource.strip_prefix(STALKER_RESOURCE_SCHEME)?.split('/');
    let input_id = parts.next()?.parse().ok()?;
    let cluster = parts.next()?.parse().ok()?;
    let provider_id = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some((input_id, cluster, provider_id))
}

async fn stalker_resource_response(
    app_state: &Arc<AppState>,
    input_id: u16,
    cluster: XtreamCluster,
    provider_id: u32,
) -> axum::response::Response {
    let Some(input) = app_state.app_config.get_input_by_id(input_id).filter(|input| input.input_type.is_stalker()) else {
        return axum::http::StatusCode::NOT_FOUND.into_response();
    };
    let kind = match cluster {
        XtreamCluster::Live => StalkerStreamKind::Live,
        XtreamCluster::Video => StalkerStreamKind::Movie,
        XtreamCluster::Series => StalkerStreamKind::Episode,
    };
    let client = app_state.http_client.load().as_ref().clone();
    match re_resolve_stalker_url(&app_state.app_config, &client, &input, provider_id, kind, false).await {
        Ok(Some(resolved_url)) => {
            let Ok(url) = Url::parse(&resolved_url) else {
                return axum::http::StatusCode::BAD_GATEWAY.into_response();
            };
            if validate_public_playable_url(&url).await.is_err() {
                return axum::http::StatusCode::BAD_GATEWAY.into_response();
            }
            axum::response::Redirect::temporary(url.as_str()).into_response()
        }
        Ok(None) => axum::http::StatusCode::NOT_FOUND.into_response(),
        Err(err) => {
            error!("Failed to resolve Stalker preview stream: {}", sanitize_sensitive_info(&err.to_string()));
            axum::http::StatusCode::BAD_GATEWAY.into_response()
        }
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

const FILTER_PREVIEW_DEFAULT_SAMPLES: u16 = 25;
const FILTER_PREVIEW_MAX_SAMPLES: u16 = 50;

#[derive(serde::Deserialize)]
struct FilterPreviewRequest {
    target: u16,
    filter: String,
    #[serde(default)]
    limit: Option<u16>,
    #[serde(default)]
    match_as_ascii: bool,
}

#[derive(serde::Serialize)]
struct FilterPreviewItem {
    name: String,
    group: String,
    item_type: String,
}

impl From<&PlaylistItem> for FilterPreviewItem {
    fn from(pli: &PlaylistItem) -> Self {
        let header = &pli.header;
        Self {
            name: header.name.to_string(),
            group: header.group.to_string(),
            item_type: header.item_type.as_str().to_string(),
        }
    }
}

#[derive(serde::Serialize, Default)]
struct FilterPreviewClusterStats {
    total: usize,
    matched: usize,
}

#[derive(serde::Serialize, Default)]
struct FilterPreviewResponse {
    total: usize,
    matched: usize,
    live: FilterPreviewClusterStats,
    vod: FilterPreviewClusterStats,
    series: FilterPreviewClusterStats,
    sample_matched: Vec<FilterPreviewItem>,
    sample_excluded: Vec<FilterPreviewItem>,
}

impl FilterPreviewResponse {
    fn observe(&mut self, pli: &PlaylistItem, filter: &Filter, match_as_ascii: bool, sample_limit: usize) {
        let cluster_stats = match pli.header.xtream_cluster {
            XtreamCluster::Live => &mut self.live,
            XtreamCluster::Video => &mut self.vod,
            XtreamCluster::Series => &mut self.series,
        };
        self.total += 1;
        cluster_stats.total += 1;
        let provider = ValueProvider { pli, match_as_ascii };
        if filter.filter(&provider) {
            self.matched += 1;
            cluster_stats.matched += 1;
            if self.sample_matched.len() < sample_limit {
                self.sample_matched.push(FilterPreviewItem::from(pli));
            }
        } else if self.sample_excluded.len() < sample_limit {
            self.sample_excluded.push(FilterPreviewItem::from(pli));
        }
    }
}

/// Dry-run a filter DSL expression against a target's stored playlist
/// without touching processing or provider fetches.
async fn playlist_filter_preview(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(req): axum::extract::Json<FilterPreviewRequest>,
) -> impl IntoResponse + Send {
    let filter = {
        let sources = app_state.app_config.sources.load();
        match get_filter_detailed(&req.filter, sources.templates.as_deref()) {
            Ok(filter) => filter,
            Err((err, position)) => {
                return (
                    axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                    axum::Json(json!({
                        "error": err.to_string(),
                        "line": position.map(|p| p.line),
                        "column": position.map(|p| p.column),
                    })),
                )
                    .into_response()
            }
        }
    };
    let Some(target) = app_state.app_config.get_target_by_id(req.target) else {
        return (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": "Unknown target"}))).into_response();
    };
    let sample_limit = usize::from(req.limit.unwrap_or(FILTER_PREVIEW_DEFAULT_SAMPLES).min(FILTER_PREVIEW_MAX_SAMPLES));

    let mut response = FilterPreviewResponse::default();
    if target.has_output(TargetType::Xtream) {
        let mut any_cluster_read = false;
        for cluster in [XtreamCluster::Live, XtreamCluster::Video, XtreamCluster::Series] {
            if let Some(mut iterator) = iter_raw_xtream_target_playlist(&app_state.app_config, &target, cluster).await
            {
                any_cluster_read = true;
                while let Some(entry) = iterator.next().await {
                    match entry {
                        Ok(item) => {
                            let pli = PlaylistItem::from(&item);
                            response.observe(&pli, &filter, req.match_as_ascii, sample_limit);
                        }
                        Err(err) => {
                            error!("Filter preview failed to read stored {cluster} playlist: {err}");
                            return (
                                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                axum::Json(json!({"error": "Failed to read stored playlist"})),
                            )
                                .into_response();
                        }
                    }
                }
            }
        }
        if !any_cluster_read {
            return (
                axum::http::StatusCode::NOT_FOUND,
                axum::Json(json!({"error": "Stored playlist is not available, update the playlist first"})),
            )
                .into_response();
        }
    } else if target.has_output(TargetType::M3u) {
        let Some(mut iterator) = iter_raw_m3u_target_playlist(&app_state.app_config, &target, None).await else {
            return (
                axum::http::StatusCode::NOT_FOUND,
                axum::Json(json!({"error": "Stored playlist is not available, update the playlist first"})),
            )
                .into_response();
        };
        while let Some(entry) = iterator.next().await {
            match entry {
                Ok(item) => {
                    let pli = PlaylistItem::from(&item);
                    response.observe(&pli, &filter, req.match_as_ascii, sample_limit);
                }
                Err(err) => {
                    error!("Filter preview failed to read stored m3u playlist: {err}");
                    return (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        axum::Json(json!({"error": "Failed to read stored playlist"})),
                    )
                        .into_response();
                }
            }
        }
    } else {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": "Target has no xtream or m3u output to preview"})),
        )
            .into_response();
    }

    axum::Json(response).into_response()
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
        .route("/playlist/filter/preview", axum::routing::post(playlist_filter_preview))
}

pub fn v1_api_playlist_register_public(router: Router<Arc<AppState>>) -> axum::Router<Arc<AppState>> {
    router
        .route("/playlist/resource/{resource}", axum::routing::get(playlist_resource))
        .route(
            "/playlist/webplayer/{token}/{target_id}/{cluster}/{stream_id}",
            axum::routing::get(playlist_webplayer_stream),
        )
        .route(
            "/playlist/recording/{token}/{cluster}/{virtual_id}",
            axum::routing::get(playlist_recording_stream),
        )
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
        .route("/filter/preview", axum::routing::post(playlist_filter_preview))
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
    use super::{resolve_provider_url_for_request, resolve_recording_config};
    use crate::{
        api::model::{
            ActiveProviderManager, ActiveUserManager, AppState, ConnectionManager, DownloadQueue, EventManager,
            MetadataUpdateManager, PlaylistStorageState, SharedStreamManager,
        },
        model::{
            AppConfig, Config, ConfigInput, ConfigProvider, ConfigSource, ConfigTarget, SourcesConfig,
            StreamHistoryConfig, VideoDownloadConfig,
        },
        utils::{
            epg::{get_input_raw_epg_file_path, get_input_raw_xmltv_file_path},
            GeoIp,
        },
    };
    use arc_swap::{ArcSwap, ArcSwapOption};
    use axum::{
        extract::{Path as AxumPath, Query, State},
        body::Body,
        http::{Request, StatusCode},
        response::IntoResponse,
        Json, Router,
    };
    use chrono::Utc;
    use crate::model::ConfigInputOptions;
    use serde_json::json;
    use shared::{
        foundation::Filter,
        model::{
            provider_saturation::build_group_lookup, ConfigPaths, ConfigProviderDto, EpgChannel, EpgConfigDto,
            EpgProgramme, EpgSourceDto, EpgSourceTypeDto, IcsDummyConfigDto, IcsEpgSourceConfigDto, InputType,
            PlaylistRequest, ProcessingOrder, XtreamCluster,
        },
        utils::Internable,
    };
    use std::{collections::HashMap, sync::Arc};
    use tempfile::tempdir;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;
    use tower::ServiceExt;
    use url::Url;

    #[tokio::test]
    async fn stable_recording_route_rejects_target_and_input_from_different_sources() {
        let app_config = Arc::new(test_app_config(
            Arc::new(ConfigInput { id: 7, name: "input-a".intern(), ..Default::default() }),
            ConfigSource {
                inputs: vec!["input-a".intern()],
                targets: vec![Arc::new(ConfigTarget {
                    id: 11,
                    enabled: true,
                    name: "stable-target".to_string(),
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
                })],
            },
        ));
        let app_state = test_app_state(Arc::clone(&app_config));
        let token = crate::auth::create_access_token(&app_config.access_token_secret, 1);
        let fingerprint = crate::auth::Fingerprint::new(
            "test".to_string(),
            "127.0.0.1".to_string(),
            "127.0.0.1:1234".parse().expect("test socket address"),
        );

        let response = super::playlist_recording_stream(
            fingerprint,
            AxumPath((token, "live".to_string(), 42)),
            Query(super::RecordingStreamQuery {
                target_name: "stable-target".to_string(),
                input_name: "input-b".to_string(),
            }),
            State(app_state),
            axum::http::HeaderMap::new(),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    /// Generate an XMLTV datetime string in the format `YYYYMMDDHHmmss +0000`
    /// offset by `hours_from_now` hours from the current time.
    fn epg_dt(hours_from_now: i64) -> String {
        let dt = Utc::now() + chrono::Duration::hours(hours_from_now);
        dt.format("%Y%m%d%H%M%S %z").to_string()
    }

    fn xmltv_source_dto(url: &str, priority: i16) -> EpgSourceDto {
        EpgSourceDto { url: url.to_string(), priority, ..EpgSourceDto::default() }
    }

    fn ics_source_dto(url: &str, channel_id: &str, priority: i16) -> EpgSourceDto {
        EpgSourceDto {
            source_type: EpgSourceTypeDto::Ics,
            url: url.to_string(),
            priority,
            channel_id: Some(channel_id.to_string()),
            channel_title: Some("Formula 1".to_string()),
            ics: Some(IcsEpgSourceConfigDto::default()),
            ..EpgSourceDto::default()
        }
    }

    fn test_app_config(input: Arc<ConfigInput>, source: ConfigSource) -> AppConfig {
        let inputs = vec![input];
        let sources = SourcesConfig {
            batch_files: vec![],
            provider: vec![],
            group_lookup: build_group_lookup(&inputs),
            inputs,
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

    #[test]
    fn recording_target_resolution_uses_stable_name_and_input_across_runtime_ids() {
        let input_a = Arc::new(ConfigInput { id: 7, name: "input-a".intern(), ..Default::default() });
        let input_b = Arc::new(ConfigInput { id: 8, name: "input-b".intern(), ..Default::default() });
        let target = |id, name: &str| Arc::new(ConfigTarget {
            id,
            enabled: true,
            name: name.to_string(),
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
        let sources = |later_target_id| {
            let inputs = vec![Arc::clone(&input_a), Arc::clone(&input_b)];
            SourcesConfig {
                batch_files: vec![],
                provider: vec![],
                group_lookup: build_group_lookup(&inputs),
                inputs,
                sources: vec![
                    ConfigSource { inputs: vec!["input-a".intern()], targets: vec![target(11, "target-a")] },
                    ConfigSource {
                        inputs: vec!["input-b".intern()],
                        targets: vec![target(later_target_id, "stable-target")],
                    },
                ],
                templates: None,
            }
        };
        let mut app_config = test_app_config(
            Arc::new(ConfigInput { id: 0, name: "unused".intern(), ..Default::default() }),
            ConfigSource { inputs: vec![], targets: vec![] },
        );
        app_config.sources = Arc::new(ArcSwap::from_pointee(sources(12)));

        let snapshot = app_config.sources.load();
        let resolved = resolve_recording_config(snapshot.as_ref(), "stable-target", "input-b")
            .expect("stable source in first snapshot");
        assert_eq!(resolved.target.id, 12);
        assert_eq!(resolved.input.name.as_ref(), "input-b");
        assert!(resolve_recording_config(snapshot.as_ref(), "stable-target", "input-a").is_none());
        drop(snapshot);

        app_config.sources.store(Arc::new(sources(37)));
        let snapshot = app_config.sources.load();
        let resolved = resolve_recording_config(snapshot.as_ref(), "stable-target", "input-b")
            .expect("stable source after reload");
        assert_eq!(resolved.target.id, 37);
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
            hls_cache: CancellationToken::new(),
        };
        let metadata_manager = Arc::new(MetadataUpdateManager::new(tokens.metadata.clone()));
        let (manual_update_sender, _) = mpsc::channel::<crate::api::model::ManualPlaylistUpdateRequest>(1);

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
            public_http_client_no_redirect: Arc::new(ArcSwap::from_pointee(reqwest::Client::new())),
            downloads: Arc::new(crate::api::model::DownloadQueue::new()),
            cache: Arc::new(ArcSwapOption::default()),
            shared_stream_manager,
            hls_proxy: Arc::new(crate::api::model::HlsProxyManager::new()),
            hls_provisioning: Arc::new(crate::api::model::HlsProvisioningState::new()),
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

    async fn stalker_mock_handler(Query(params): Query<HashMap<String, String>>) -> impl IntoResponse {
        let action = params.get("action").map_or("", String::as_str);
        let portal_type = params.get("type").map_or("", String::as_str);
        let page = params.get("p").map_or("1", String::as_str);
        let response = match (portal_type, action, page) {
            ("stb", "handshake", _) => json!({"js": {"token": "preview-token"}}),
            ("stb", "get_profile", _) => json!({"js": {"status": 1, "max_connections": 1}}),
            ("stb", "get_capabilities", _) => json!({"js": {}}),
            ("itv", "get_genres", _) => json!({"js": [{"id": "10", "title": "News"}]}),
            ("itv", "get_ordered_list", "1") => json!({
                "js": {
                    "data": {
                        "101": {
                            "id": "101",
                            "name": "Demo Channel",
                            "category_id": "10",
                            "cmd": "ffmpeg http://streams.example/live/101"
                        },
                        "102": {
                            "id": "102",
                            "name": "Private Channel",
                            "category_id": "10",
                            "cmd": "ffmpeg http://streams.example/live/102"
                        }
                    }
                }
            }),
            ("itv", "create_link", _) => {
                let destination = if params.get("cmd").is_some_and(|cmd| cmd.ends_with("/102")) {
                    "http://127.0.0.1/live/102"
                } else {
                    "http://8.8.8.8/live/101"
                };
                json!({"js": {"cmd": format!("ffmpeg {destination}")}})
            }
            _ => json!({"js": []}),
        };
        Json(response)
    }

    async fn spawn_stalker_mock_server() -> (String, tokio::task::JoinHandle<()>) {
        let router = Router::new().route("/server/load.php", axum::routing::get(stalker_mock_handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind mock stalker server");
        let base_url = format!("http://{}", listener.local_addr().expect("mock addr"));
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.expect("serve mock stalker server");
        });
        (base_url, handle)
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
        let inputs = vec![input_a, input_b];
        let sources = SourcesConfig {
            batch_files: vec![],
            provider: vec![],
            group_lookup: build_group_lookup(&inputs),
            inputs,
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

        assert_eq!(live, "http://player.example/api/v1/playlist/webplayer/token123/1/live/42");
        assert_eq!(movie, "http://player.example/api/v1/playlist/webplayer/token123/1/movie/42");
        assert_eq!(series, "http://player.example/api/v1/playlist/webplayer/token123/1/series/42");
    }

    #[test]
    fn build_recording_stream_url_encodes_stable_names_without_runtime_id() {
        let url = super::build_recording_stream_url(
            "http://player.example/base",
            "token123",
            "News/HD &+",
            "input/name ?+",
            42,
            XtreamCluster::Live,
        )
        .expect("valid recording url");
        let parsed = Url::parse(&url).expect("parse recording url");
        let query = parsed.query_pairs().collect::<HashMap<_, _>>();

        assert_eq!(parsed.path(), "/base/api/v1/playlist/recording/token123/live/42");
        assert_eq!(query.get("target_name").map(std::convert::AsRef::as_ref), Some("News/HD &+"));
        assert_eq!(query.get("input_name").map(std::convert::AsRef::as_ref), Some("input/name ?+"));
        assert!(!parsed.path().contains("/11/"));
    }

    #[test]
    fn recording_source_descriptor_is_token_free_and_percent_encodes_names() {
        let url = super::build_recording_source_descriptor(
            "News/HD &+",
            "input/name ?+",
            42,
            XtreamCluster::Video,
        )
        .expect("valid recording source descriptor");
        let parsed = Url::parse(&url).expect("parse recording source descriptor");
        let query = parsed.query_pairs().collect::<HashMap<_, _>>();

        assert_eq!(parsed.scheme(), "tuliprox-recording");
        assert_eq!(parsed.host_str(), Some("source"));
        assert_eq!(query.get("target_name").map(std::convert::AsRef::as_ref), Some("News/HD &+"));
        assert_eq!(query.get("input_name").map(std::convert::AsRef::as_ref), Some("input/name ?+"));
        assert_eq!(query.get("virtual_id").map(std::convert::AsRef::as_ref), Some("42"));
        assert_eq!(query.get("cluster").map(std::convert::AsRef::as_ref), Some("movie"));
        assert!(!url.contains("token"));
    }

    #[test]
    fn future_scheduled_recording_descriptor_round_trips_without_token() {
        let url = super::build_recording_source_descriptor(
            "stable-target",
            "input-a",
            42,
            XtreamCluster::Live,
        )
        .expect("valid recording url");
        let download_cfg = VideoDownloadConfig {
            directory: "/tmp".to_string(),
            organize_into_directories: false,
            episode_pattern: None,
            headers: HashMap::new(),
            download_priority: 0,
            recording_priority: 0,
            reserve_slots_for_users: 0,
            max_background_per_provider: 0,
            retry_backoff_initial_secs: 3,
            retry_backoff_multiplier: 3.0,
            retry_backoff_max_secs: 30,
            retry_backoff_jitter_percent: 0,
            retry_max_attempts: 5,
            recording: None,
        };
        let recording = crate::api::model::FileDownload::new_recording(
            &url,
            "recording.ts",
            &download_cfg,
            1_700_000_000,
            3600,
            Some("input-a".intern()),
            0,
        )
        .expect("valid recording task");
        let persisted = DownloadQueue::to_persisted(&recording);
        let restored = DownloadQueue::from_persisted(persisted.clone()).expect("restore recording task");

        assert_eq!(persisted.url, url);
        assert_eq!(restored.url.as_str(), url);
        assert!(persisted.url.starts_with("tuliprox-recording://source?"));
        assert!(!persisted.url.contains("token"));
        assert!(!persisted.url.contains("/11/"));
        assert!(persisted.url.contains("target_name=stable-target"));
    }

    #[test]
    fn merge_epg_channels_prefers_higher_priority_metadata_and_fills_lower_priority_gaps() {
        let low_priority = EpgChannel {
            id: "demo.channel".intern(),
            title: Some("Low".intern()),
            icon: Some("http://low/icon.png".intern()),
            programmes: vec![EpgProgramme::new_all(
                10,
                20,
                "demo.channel".intern(),
                Some("Low Show".intern()),
                None,
                None,
            )],
        };
        let high_priority = EpgChannel {
            id: "demo.channel".intern(),
            title: Some("High".intern()),
            icon: Some("http://high/icon.png".intern()),
            programmes: vec![EpgProgramme::new_all(
                30,
                40,
                "demo.channel".intern(),
                Some("High Show".intern()),
                None,
                None,
            )],
        };
        let same_priority = EpgChannel {
            id: "demo.channel".intern(),
            title: Some("Same".intern()),
            icon: Some("http://same/icon.png".intern()),
            programmes: vec![
                EpgProgramme::new_all(30, 40, "demo.channel".intern(), Some("Duplicate".intern()), None, None),
                EpgProgramme::new_all(50, 60, "demo.channel".intern(), Some("Second Show".intern()), None, None),
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
                sources: Some(vec![xmltv_source_dto("provider://demo/xmltv.php?username=user&password=pass", 0)]),
                t_sources: vec![xmltv_source_dto("provider://demo/xmltv.php?username=user&password=pass", 0)],
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
        let raw_epg_path = get_input_raw_xmltv_file_path(
            "provider://demo/xmltv.php?username=user&password=pass",
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
    async fn load_epg_channels_for_input_reads_cached_ics_source() {
        let temp_dir = tempdir().expect("temp dir");
        let ics_dto = ics_source_dto("https://example.com/f1.ics", "f1.calendar", -10);
        let input = Arc::new(ConfigInput {
            id: 7,
            name: "input".intern(),
            epg: Some(crate::model::EpgConfig::from(&EpgConfigDto {
                sources: Some(vec![ics_dto.clone()]),
                t_sources: vec![ics_dto],
                smart_match: None,
            })),
            ..Default::default()
        });
        let source = ConfigSource { inputs: vec![Arc::clone(&input.name)], targets: vec![] };
        let app_config = test_app_config(input.clone(), source);
        app_config.config.store(Arc::new(Config {
            storage_dir: temp_dir.path().to_string_lossy().to_string(),
            ..Default::default()
        }));
        let epg_source = &input.epg.as_ref().expect("epg").sources[0];
        let raw_epg_path =
            get_input_raw_epg_file_path(epg_source, input.as_ref(), temp_dir.path().to_string_lossy().as_ref())
                .await
                .expect("epg path");
        if let Some(parent) = raw_epg_path.parent() {
            tokio::fs::create_dir_all(parent).await.expect("epg dir");
        }
        tokio::fs::write(
            &raw_epg_path,
            "BEGIN:VCALENDAR\nBEGIN:VEVENT\nSUMMARY:Practice 1\nDTSTART:20260306T123000Z\nDTEND:20260306T133000Z\nEND:VEVENT\nEND:VCALENDAR",
        )
        .await
        .expect("write ics");

        let app_state = test_app_state(Arc::new(app_config));
        let channels =
            super::load_epg_channels_for_input(&app_state, input.as_ref()).await.expect("load epg").expect("channels");

        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0].id.as_ref(), "f1.calendar");
        assert_eq!(channels[0].title.as_deref(), Some("Formula 1"));
        assert_eq!(channels[0].programmes[0].title.as_deref(), Some("Practice 1"));
    }

    #[tokio::test]
    async fn load_epg_channels_for_input_redownloads_invalid_cached_ics_source() {
        let temp_dir = tempdir().expect("temp dir");
        tokio::fs::write(
            temp_dir.path().join("valid.ics"),
            "BEGIN:VCALENDAR\nBEGIN:VEVENT\nSUMMARY:Downloaded\nDTSTART:20260306T123000Z\nDTEND:20260306T133000Z\nEND:VEVENT\nEND:VCALENDAR",
        )
        .await
        .expect("write valid ics source");
        let ics_dto = ics_source_dto("valid.ics", "f1.calendar", -10);
        let input = Arc::new(ConfigInput {
            id: 7,
            name: "input".intern(),
            epg: Some(crate::model::EpgConfig::from(&EpgConfigDto {
                sources: Some(vec![ics_dto.clone()]),
                t_sources: vec![ics_dto],
                smart_match: None,
            })),
            ..Default::default()
        });
        let source = ConfigSource { inputs: vec![Arc::clone(&input.name)], targets: vec![] };
        let app_config = test_app_config(input.clone(), source);
        app_config.config.store(Arc::new(Config {
            storage_dir: temp_dir.path().to_string_lossy().to_string(),
            ..Default::default()
        }));
        let epg_source = &input.epg.as_ref().expect("epg").sources[0];
        let raw_epg_path =
            get_input_raw_epg_file_path(epg_source, input.as_ref(), temp_dir.path().to_string_lossy().as_ref())
                .await
                .expect("epg path");
        if let Some(parent) = raw_epg_path.parent() {
            tokio::fs::create_dir_all(parent).await.expect("epg dir");
        }
        tokio::fs::write(&raw_epg_path, "upstream returned an error page").await.expect("write invalid cache");

        let app_state = test_app_state(Arc::new(app_config));
        let channels =
            super::load_epg_channels_for_input(&app_state, input.as_ref()).await.expect("load epg").expect("channels");

        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0].programmes[0].title.as_deref(), Some("Downloaded"));
        assert_eq!(
            tokio::fs::read_to_string(&raw_epg_path).await.expect("updated cache"),
            "BEGIN:VCALENDAR\nBEGIN:VEVENT\nSUMMARY:Downloaded\nDTSTART:20260306T123000Z\nDTEND:20260306T133000Z\nEND:VEVENT\nEND:VCALENDAR"
        );
    }

    #[tokio::test]
    async fn load_epg_channels_for_input_preserves_invalid_cache_when_refresh_fails() {
        let temp_dir = tempdir().expect("temp dir");
        let invalid_cache = "upstream returned an error page";
        let ics_dto = ics_source_dto("missing.ics", "f1.calendar", -10);
        let input = Arc::new(ConfigInput {
            id: 7,
            name: "input".intern(),
            epg: Some(crate::model::EpgConfig::from(&EpgConfigDto {
                sources: Some(vec![ics_dto.clone()]),
                t_sources: vec![ics_dto],
                smart_match: None,
            })),
            ..Default::default()
        });
        let source = ConfigSource { inputs: vec![Arc::clone(&input.name)], targets: vec![] };
        let app_config = test_app_config(input.clone(), source);
        app_config.config.store(Arc::new(Config {
            storage_dir: temp_dir.path().to_string_lossy().to_string(),
            ..Default::default()
        }));
        let epg_source = &input.epg.as_ref().expect("epg").sources[0];
        let raw_epg_path =
            get_input_raw_epg_file_path(epg_source, input.as_ref(), temp_dir.path().to_string_lossy().as_ref())
                .await
                .expect("epg path");
        if let Some(parent) = raw_epg_path.parent() {
            tokio::fs::create_dir_all(parent).await.expect("epg dir");
        }
        tokio::fs::write(&raw_epg_path, invalid_cache).await.expect("write invalid cache");

        let app_state = test_app_state(Arc::new(app_config));
        assert!(super::load_epg_channels_for_input(&app_state, input.as_ref()).await.is_err());
        assert_eq!(tokio::fs::read_to_string(&raw_epg_path).await.expect("preserved cache"), invalid_cache);
    }

    #[tokio::test]
    async fn load_epg_channels_for_input_applies_ics_dummy_policy_in_preview() {
        let temp_dir = tempdir().expect("temp dir");
        tokio::fs::write(temp_dir.path().join("empty.ics"), "BEGIN:VCALENDAR\nEND:VCALENDAR")
            .await
            .expect("write empty ics source");
        let mut ics_dto = ics_source_dto("empty.ics", "f1.calendar", -10);
        ics_dto.ics = Some(IcsEpgSourceConfigDto {
            dummy: IcsDummyConfigDto {
                enabled: true,
                title: "No F1".to_string(),
                days_past: 0,
                days_future: 0,
                block_hours: 24,
                ..IcsDummyConfigDto::default()
            },
            ..IcsEpgSourceConfigDto::default()
        });
        let input = Arc::new(ConfigInput {
            id: 7,
            name: "input".intern(),
            epg: Some(crate::model::EpgConfig::from(&EpgConfigDto {
                sources: Some(vec![ics_dto.clone()]),
                t_sources: vec![ics_dto],
                smart_match: None,
            })),
            ..Default::default()
        });
        let source = ConfigSource { inputs: vec![Arc::clone(&input.name)], targets: vec![] };
        let app_config = test_app_config(input.clone(), source);
        app_config.config.store(Arc::new(Config {
            storage_dir: temp_dir.path().to_string_lossy().to_string(),
            ..Default::default()
        }));

        let app_state = test_app_state(Arc::new(app_config));
        let channels =
            super::load_epg_channels_for_input(&app_state, input.as_ref()).await.expect("load epg").expect("channels");

        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0].id.as_ref(), "f1.calendar");
        assert!(!channels[0].programmes.is_empty());
        assert!(channels[0].programmes.iter().all(|programme| programme.title.as_deref() == Some("No F1")));
    }

    #[tokio::test]
    async fn load_epg_channels_for_input_prefers_high_priority_ics_dummy_policy() {
        let temp_dir = tempdir().expect("temp dir");
        for filename in ["low.ics", "high.ics"] {
            tokio::fs::write(temp_dir.path().join(filename), "BEGIN:VCALENDAR\nEND:VCALENDAR")
                .await
                .expect("write empty ics source");
        }

        let mut low_priority = ics_source_dto("low.ics", "f1.calendar", 10);
        low_priority.ics = Some(IcsEpgSourceConfigDto {
            dummy: IcsDummyConfigDto {
                enabled: true,
                title: "Low priority".to_string(),
                days_past: 0,
                days_future: 0,
                block_hours: 24,
                ..IcsDummyConfigDto::default()
            },
            ..IcsEpgSourceConfigDto::default()
        });
        let mut high_priority = ics_source_dto("high.ics", "f1.calendar", -10);
        high_priority.ics = Some(IcsEpgSourceConfigDto {
            dummy: IcsDummyConfigDto {
                enabled: true,
                title: "High priority".to_string(),
                days_past: 0,
                days_future: 0,
                block_hours: 24,
                ..IcsDummyConfigDto::default()
            },
            ..IcsEpgSourceConfigDto::default()
        });
        let input = Arc::new(ConfigInput {
            id: 7,
            name: "input".intern(),
            epg: Some(crate::model::EpgConfig::from(&EpgConfigDto {
                sources: Some(vec![low_priority.clone(), high_priority.clone()]),
                t_sources: vec![low_priority, high_priority],
                smart_match: None,
            })),
            ..Default::default()
        });
        let source = ConfigSource { inputs: vec![Arc::clone(&input.name)], targets: vec![] };
        let app_config = test_app_config(input.clone(), source);
        app_config.config.store(Arc::new(Config {
            storage_dir: temp_dir.path().to_string_lossy().to_string(),
            ..Default::default()
        }));

        let app_state = test_app_state(Arc::new(app_config));
        let channels =
            super::load_epg_channels_for_input(&app_state, input.as_ref()).await.expect("load epg").expect("channels");

        assert_eq!(channels.len(), 1);
        assert!(!channels[0].programmes.is_empty());
        assert!(channels[0].programmes.iter().all(|programme| programme.title.as_deref() == Some("High priority")));
    }

    #[tokio::test]
    async fn playlist_epg_custom_route_rejects_invalid_url_scheme() {
        let input = Arc::new(ConfigInput { id: 7, name: "input".intern(), ..Default::default() });
        let source = ConfigSource { inputs: vec![Arc::clone(&input.name)], targets: vec![] };
        let app_state = test_app_state(Arc::new(test_app_config(input, source)));
        let router = super::v1_api_playlist_register_protected(Router::new()).with_state(app_state);
        let request = Request::builder()
            .method("POST")
            .uri("/playlist/epg")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"Custom":"ftp://example.com/epg.xml"}"#))
            .expect("request");

        let response = router.into_service::<Body>().oneshot(request).await.expect("response");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn playlist_live_input_route_supports_stalker_inputs() {
        let temp_dir = tempdir().expect("temp dir");
        let (base_url, server_handle) = spawn_stalker_mock_server().await;
        let input = Arc::new(ConfigInput {
            id: 7,
            name: "stalker".intern(),
            input_type: InputType::Stalker,
            url: base_url,
            enabled: true,
            options: Some(ConfigInputOptions {
                flags: crate::model::ConfigInputFlagsSet::new(),
                resolve_delay: shared::defaults::default_resolve_delay_secs(),
                probe_delay: shared::defaults::default_probe_delay_secs(),
                probe_live_interval_hours: 120,
                resolve_filter: None,
                probe_filter: None,
            }),
            stalker: Some(crate::model::StalkerInputConfig {
                device: None,
                auth_mode: shared::model::StalkerAuthMode::Auto,
                mag_preset: shared::model::StalkerMagPreset::GenericSafe,
                endpoint_preference: shared::model::StalkerEndpointPreference::ServerLoad,
                size_caps: None,
                catalog_max_pages: None,
                ..Default::default()
            }),
            ..Default::default()
        });
        let source = ConfigSource {
            inputs: vec![Arc::clone(&input.name)],
            targets: vec![],
        };
        let app_config = test_app_config(Arc::clone(&input), source);
        let mut sources = app_config.sources.load().as_ref().clone();
        sources.inputs.push(Arc::new(ConfigInput {
            id: 8,
            name: "m3u".intern(),
            input_type: InputType::M3u,
            ..Default::default()
        }));
        app_config.sources.store(Arc::new(sources));
        app_config.config.store(Arc::new(Config {
            storage_dir: temp_dir.path().to_string_lossy().to_string(),
            ..Default::default()
        }));
        let app_state = test_app_state(Arc::new(app_config));
        let router = super::v1_api_playlist_register_protected(super::v1_api_playlist_register_public(Router::new()))
            .with_state(Arc::clone(&app_state));
        let request = Request::builder()
            .method("POST")
            .uri("/playlist/live")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"Input":"stalker"}"#))
            .expect("request");

        let response = router.clone().into_service::<Body>().oneshot(request).await.expect("response");

        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("body");
        let body_text = String::from_utf8(body.to_vec()).expect("utf8");
        assert_eq!(status, StatusCode::OK, "{body_text}");
        assert!(body_text.contains("Demo Channel"), "{body_text}");
        assert!(!body_text.contains("ffmpeg http://streams.example/live/101"), "{body_text}");
        let body_json: serde_json::Value = serde_json::from_str(&body_text).unwrap_or_default();
        let playback_url = body_json
            .as_array()
            .and_then(|items| items.first())
            .and_then(serde_json::Value::as_array)
            .and_then(|item| item.get(6))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        assert!(!playback_url.is_empty(), "{body_text}");
        assert!(playback_url.starts_with("http") || playback_url.starts_with('/'), "{playback_url}");

        let resource_path = playback_url
            .find("/playlist/resource/")
            .map(|index| &playback_url[index..])
            .expect("Stalker playback resource path");
        let response = router
            .clone()
            .into_service::<Body>()
            .oneshot(Request::get(resource_path).body(Body::empty()).expect("resource request"))
            .await
            .expect("resource response");
        assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT, "{resource_path}");
        assert_eq!(response.headers().get("location").and_then(|value| value.to_str().ok()), Some("http://8.8.8.8/live/101"));

        let response = router
            .clone()
            .into_service::<Body>()
            .oneshot(Request::get("/playlist/resource/*").body(Body::empty()).expect("malformed request"))
            .await
            .expect("malformed response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        for locator in [
            format!("{}99/live/101", super::STALKER_RESOURCE_SCHEME),
            format!("{}8/live/101", super::STALKER_RESOURCE_SCHEME),
        ] {
            let resource = shared::utils::obfuscate_text(&app_state.get_encrypt_secret(), &locator);
            let response = router
                .clone()
                .into_service::<Body>()
                .oneshot(
                    Request::get(format!("/playlist/resource/{resource}"))
                        .body(Body::empty())
                        .expect("not-found request"),
                )
                .await
                .expect("not-found response");
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{locator}");
        }

        let resource = shared::utils::obfuscate_text(
            &app_state.get_encrypt_secret(),
            &format!("{}7/live/102", super::STALKER_RESOURCE_SCHEME),
        );
        let response = router
            .into_service::<Body>()
            .oneshot(
                Request::get(format!("/playlist/resource/{resource}"))
                    .body(Body::empty())
                    .expect("private destination request"),
            )
            .await
            .expect("private destination response");
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        server_handle.abort();
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
                sources: Some(vec![xmltv_source_dto("provider://demo/xmltv.php?username=user&password=pass", 0)]),
                t_sources: vec![xmltv_source_dto("provider://demo/xmltv.php?username=user&password=pass", 0)],
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
        let raw_epg_path = get_input_raw_xmltv_file_path(
            "provider://demo/xmltv.php?username=user&password=pass",
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
                    xmltv_source_dto(secondary_url, 10),
                    xmltv_source_dto(primary_url, 0),
                    xmltv_source_dto("provider://demo/xmltv-same-priority.php?username=user&password=pass", 0),
                ]),
                t_sources: vec![
                    xmltv_source_dto(secondary_url, 10),
                    xmltv_source_dto(primary_url, 0),
                    xmltv_source_dto("provider://demo/xmltv-same-priority.php?username=user&password=pass", 0),
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

        let primary_path =
            get_input_raw_xmltv_file_path(primary_url, input.as_ref(), temp_dir.path().to_string_lossy().as_ref())
                .await
                .expect("primary epg path");
        let secondary_path =
            get_input_raw_xmltv_file_path(secondary_url, input.as_ref(), temp_dir.path().to_string_lossy().as_ref())
                .await
                .expect("secondary epg path");
        let same_priority_path = get_input_raw_xmltv_file_path(
            "provider://demo/xmltv-same-priority.php?username=user&password=pass",
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
