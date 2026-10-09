use super::{build_playlist_webplayer_url, ResolvedRecordingSource};
use crate::{
    api::{
        api_utils::try_result_bad_request,
        endpoints::{
            m3u_api::m3u_api_stream_loaded,
            xtream_api::{xtream_player_api_stream_with_resolved_target, ApiStreamContext, ApiStreamRequest},
        },
        model::{
            recording::recording_source_resolution::{resolve_recording_config, resolve_recording_target},
            AppState,
        },
    },
    auth::{create_access_token, verify_access_token},
    model::ConfigTarget,
    repository::{iter_raw_m3u_target_playlist, iter_raw_xtream_target_playlist, m3u_get_item_for_stream_id},
    utils::{canonicalize_output_epg_id, canonicalize_untrusted_epg_id, EpgIdOutputCase},
};
use axum::response::IntoResponse;
use log::{debug, error};
use serde::Deserialize;
use shared::model::{ConfigTargetOptions, PlaylistItemType, TargetType, VirtualId, XtreamCluster};
use std::{str::FromStr, sync::Arc};
use tokio_stream::StreamExt;

pub(in crate::api) fn build_webplayer_recording_url(
    app_config: &crate::model::AppConfig,
    target_id: u16,
    virtual_id: u32,
    cluster: XtreamCluster,
) -> Option<String> {
    let access_token = create_access_token(&app_config.access_token_secret, 30, crate::auth::scope::INTERNAL_PLAYER);
    let config = app_config.config.load();
    let server_name = config
        .web_ui
        .as_ref()
        .and_then(|web_ui| web_ui.player_server.as_ref())
        .map_or("default", |server_name| server_name.as_str());
    let server_info = app_config.get_server_info(server_name)?;
    Some(build_playlist_webplayer_url(&server_info.get_base_url(), &access_token, target_id, virtual_id, cluster))
}

/// The playlist fields a recording needs, shared by Xtream and M3U items.
pub(super) struct RecordingCandidateFields<'a> {
    pub(super) virtual_id: VirtualId,
    pub(super) input_name: &'a str,
    pub(super) name: &'a str,
    pub(super) title: &'a str,
    pub(super) group: &'a str,
    pub(super) url: &'a str,
    pub(super) item_type: PlaylistItemType,
}

fn non_blank(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

pub(super) fn recording_candidate(fields: &RecordingCandidateFields<'_>) -> ResolvedRecordingSource {
    let RecordingCandidateFields { virtual_id, input_name, name, title, group, url, item_type } = *fields;
    let is_episode = matches!(item_type, PlaylistItemType::Series | PlaylistItemType::LocalSeries);
    ResolvedRecordingSource {
        virtual_id: virtual_id.get(),
        input_name: input_name.to_string(),
        title: title.to_string(),
        group: non_blank(group),
        series_name: if is_episode { non_blank(name) } else { None },
        extension: shared::utils::extract_extension_from_url(url).map(str::to_string),
        downloadable: item_type != PlaylistItemType::SeriesInfo,
    }
}

pub(in crate::api) async fn resolve_target_recording_source(
    app_config: &crate::model::AppConfig,
    target_name: &str,
    input_name: &str,
    virtual_id: u32,
    cluster: XtreamCluster,
) -> Option<ResolvedRecordingSource> {
    // EPG grid rows carry the exact playlist id but do not know the input.
    // Resolve it from the persisted item, then validate its configured scope.
    let missing_input = input_name.trim().is_empty();
    let target = if missing_input {
        find_target_by_name(app_config, target_name)?
    } else {
        resolve_recording_target(app_config, target_name, input_name)?
    };
    let wanted = VirtualId::new(virtual_id);
    let mut resolved = None;
    if target.has_output(TargetType::Xtream) {
        if let Some(mut items) = iter_raw_xtream_target_playlist(app_config, &target, cluster).await {
            while let Some(entry) = items.next().await {
                let Ok(item) = entry else { continue };
                if item.virtual_id == wanted {
                    resolved = Some(recording_candidate(&RecordingCandidateFields {
                        virtual_id: item.virtual_id,
                        input_name: &item.input_name,
                        name: &item.name,
                        title: &item.title,
                        group: &item.group,
                        url: &item.url,
                        item_type: item.item_type,
                    }));
                    break;
                }
            }
        }
    }
    if resolved.is_none() && target.has_output(TargetType::M3u) {
        if let Some(mut items) = iter_raw_m3u_target_playlist(app_config, &target, Some(cluster)).await {
            while let Some(entry) = items.next().await {
                let Ok(item) = entry else { continue };
                if item.virtual_id == wanted {
                    resolved = Some(recording_candidate(&RecordingCandidateFields {
                        virtual_id: item.virtual_id,
                        input_name: &item.input_name,
                        name: &item.name,
                        title: &item.title,
                        group: &item.group,
                        url: &item.url,
                        item_type: item.item_type,
                    }));
                    break;
                }
            }
        }
    }
    let resolved = resolved?;
    if missing_input {
        return is_recording_input_in_target_scope(app_config, &target, &resolved.input_name).then_some(resolved);
    }
    (resolved.input_name == input_name).then_some(resolved)
}

/// `SourcesConfigDto::prepare` rejects duplicate target names across sources.
fn find_target_by_name(app_config: &crate::model::AppConfig, target_name: &str) -> Option<Arc<ConfigTarget>> {
    app_config
        .sources
        .load()
        .sources
        .iter()
        .flat_map(|source| &source.targets)
        .find(|target| target.name == target_name)
        .cloned()
}

/// Whether `input_name` belongs to the source that configures `target`.
fn is_recording_input_in_target_scope(
    app_config: &crate::model::AppConfig,
    target: &ConfigTarget,
    input_name: &str,
) -> bool {
    resolve_recording_target(app_config, &target.name, input_name).is_some_and(|configured| configured.id == target.id)
}

/// Compares a playlist item's EPG channel id against a requested one under the
/// target's configured output casing.
///
/// The web UI reads channel ids from the target EPG database, where they are
/// written in the target's output case (`lowercase_ids`). The persisted
/// playlist keeps the source casing, so a raw comparison misses every channel
/// whose source id differs in case from the EPG database key.
pub(super) fn epg_channel_id_matches(
    item_epg_channel_id: Option<&Arc<str>>,
    requested: &Arc<str>,
    output_case: EpgIdOutputCase,
) -> bool {
    item_epg_channel_id
        .is_some_and(|item_id| canonicalize_output_epg_id(item_id, output_case).as_ref() == requested.as_ref())
}

/// Picks the channel to record from every live item that carries the requested
/// EPG channel id.
///
/// Providers commonly publish one EPG id for several streams of the same
/// channel (HD, FHD, 4K, backup feeds). The web UI shows them as one EPG row
/// labelled with one of their titles, so that title is the best hint for
/// which stream the user meant. Without a matching title every candidate airs
/// the same programme, and the first one in playlist order is taken.
pub(super) fn select_epg_channel_candidate(
    candidates: Vec<ResolvedRecordingSource>,
    channel_name: Option<&str>,
) -> Option<ResolvedRecordingSource> {
    let hint = channel_name.map(str::trim).filter(|name| !name.is_empty());
    if let Some(hint) = hint {
        if let Some(index) = candidates.iter().position(|candidate| candidate.title == hint) {
            return candidates.into_iter().nth(index);
        }
        let hint = hint.to_lowercase();
        if let Some(index) = candidates.iter().position(|candidate| candidate.title.to_lowercase() == hint) {
            return candidates.into_iter().nth(index);
        }
    }
    candidates.into_iter().next()
}

pub(in crate::api) async fn resolve_target_live_recording_source_by_epg_channel(
    app_config: &crate::model::AppConfig,
    target_name: &str,
    input_name: &str,
    epg_channel_id: &str,
    channel_name: Option<&str>,
) -> Option<ResolvedRecordingSource> {
    let target = find_target_by_name(app_config, target_name)?;
    let output_case =
        EpgIdOutputCase::from_lowercase(target.options.as_ref().is_some_and(ConfigTargetOptions::lowercase_epg_ids));
    let requested = canonicalize_untrusted_epg_id(epg_channel_id, output_case);
    let mut candidates = Vec::new();
    if target.has_output(TargetType::Xtream) {
        if let Some(mut items) = iter_raw_xtream_target_playlist(app_config, &target, XtreamCluster::Live).await {
            while let Some(entry) = items.next().await {
                let Ok(item) = entry else { continue };
                if epg_channel_id_matches(item.epg_channel_id.as_ref(), &requested, output_case) {
                    candidates.push(recording_candidate(&RecordingCandidateFields {
                        virtual_id: item.virtual_id,
                        input_name: &item.input_name,
                        name: &item.name,
                        title: &item.title,
                        group: &item.group,
                        url: &item.url,
                        item_type: item.item_type,
                    }));
                }
            }
        }
    } else if target.has_output(TargetType::M3u) {
        if let Some(mut items) = iter_raw_m3u_target_playlist(app_config, &target, Some(XtreamCluster::Live)).await {
            while let Some(entry) = items.next().await {
                let Ok(item) = entry else { continue };
                if epg_channel_id_matches(item.epg_channel_id.as_ref(), &requested, output_case) {
                    candidates.push(recording_candidate(&RecordingCandidateFields {
                        virtual_id: item.virtual_id,
                        input_name: &item.input_name,
                        name: &item.name,
                        title: &item.title,
                        group: &item.group,
                        url: &item.url,
                        item_type: item.item_type,
                    }));
                }
            }
        }
    }
    // Apply input constraints before the title hint can select another feed.
    candidates.retain(|candidate| {
        (input_name.trim().is_empty() || candidate.input_name == input_name)
            && is_recording_input_in_target_scope(app_config, &target, &candidate.input_name)
    });
    select_epg_channel_candidate(candidates, channel_name)
}

#[derive(Debug, Deserialize)]
pub(super) struct RecordingStreamQuery {
    pub(super) target_name: String,
    pub(super) input_name: String,
    pub(super) provider_allocation_id: Option<u64>,
}

pub(super) async fn playlist_recording_stream(
    fingerprint: crate::auth::Fingerprint,
    axum::extract::Path((token, cluster, virtual_id)): axum::extract::Path<(String, String, u32)>,
    axum::extract::Query(query): axum::extract::Query<RecordingStreamQuery>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    req_headers: axum::http::HeaderMap,
) -> impl IntoResponse + Send {
    if !verify_access_token(&token, &app_state.app_config.access_token_secret, crate::auth::scope::INTERNAL_PLAYER) {
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
    let input = crate::api::api_utils::with_recording_headers(&app_state.app_config, resolved.input);
    if resolved.target.has_output(TargetType::Xtream) {
        let stream_id = virtual_id.to_string();
        return xtream_player_api_stream_with_resolved_target(
            &fingerprint,
            &req_headers,
            &app_state,
            resolved.target,
            super::super::xtream_api::InternalPlaybackSource::Recording(input),
            ApiStreamRequest::from_access_token(ctxt, &token, &stream_id, ""),
            query.provider_allocation_id,
        )
        .await
        .into_response();
    }
    if !resolved.target.has_output(TargetType::M3u) {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    }

    let pli = try_result_bad_request!(
        m3u_get_item_for_stream_id(virtual_id, &app_state.app_config, &app_state.playlists, &resolved.target).await,
        true,
        format!("Failed to read m3u item for stream id {virtual_id}")
    );
    if pli.input_name != input.name || pli.item_type.cluster() != ctxt.cluster() {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    }
    let user = Arc::new(crate::api::api_utils::create_recording_proxy_user(&app_state));
    m3u_api_stream_loaded(
        user,
        resolved.target,
        &fingerprint,
        &req_headers,
        &app_state,
        pli,
        input,
        None,
        None,
        query.provider_allocation_id,
    )
    .await
    .into_response()
}
