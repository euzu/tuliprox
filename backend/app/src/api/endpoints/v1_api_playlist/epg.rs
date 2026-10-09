use super::resolve_provider_url_with_input;
use crate::{
    api::{
        api_utils::json_or_bin_response,
        endpoints::{
            extract_accept_header::ExtractAcceptHeader,
            xmltv_api::{rewrite_epg_channel_resource_url, serve_epg_web_ui},
        },
        model::AppState,
    },
    model::{
        parse_xmltv_for_web_ui_from_file, parse_xmltv_for_web_ui_from_url, ConfigInput, EpgSource, EpgSourceType,
        IcsDummyPolicy,
    },
    processing::{
        epg::get_input_raw_epg_file_path,
        parser::{
            ics::parse_ics_file_to_channel,
            xmltv::{merge_epg_channels_by_priority_with_dummy_policies, EpgDummyPolicySource},
        },
    },
    utils::{file_exists_async, request},
};
use axum::response::IntoResponse;
use log::{debug, error};
use shared::{
    error::TuliproxError,
    model::{EpgChannel, PlaylistEpgRequest, TargetType},
    utils::{concat_path_leading_slash, sanitize_sensitive_info, Internable},
};
use std::{path::Path, sync::Arc};
use url::Url;

#[cfg(test)]
pub(super) fn merge_epg_channels(mut channels_by_source: Vec<(i16, Vec<EpgChannel>)>) -> Vec<EpgChannel> {
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

    parse_xmltv_for_web_ui_from_url(&app_state.app_config, &app_state.http_clients.default.load(), resolved_url).await
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

    let client = app_state.http_clients.default.load();
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

pub(super) async fn load_epg_channels_for_input(
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

pub(super) async fn playlist_epg(
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
            match parse_xmltv_for_web_ui_from_url(&app_state.app_config, &app_state.http_clients.default.load(), &url)
                .await
            {
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
