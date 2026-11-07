use super::blocks_new_playback;
use crate::{
    api::model::AppState,
    model::{ConfigInput, ProxyUserCredentials},
    processing::processor::re_resolve_stalker_url,
};
use axum::http::StatusCode;
use log::error;
use shared::{
    error::TuliproxError,
    model::{InputType, PlaylistItemType, StalkerStreamKind, XtreamCluster},
    utils::sanitize_sensitive_info,
};
use std::sync::Arc;

pub(super) fn should_refresh_stalker_playback(
    input_type: InputType,
    request_url_valid: bool,
    status: Option<StatusCode>,
) -> bool {
    input_type.is_stalker() && (!request_url_valid || status.is_some_and(|status| status.is_client_error()))
}

pub(super) fn needs_initial_stalker_resolution(input_type: InputType, stream_url: &str) -> bool {
    input_type.is_stalker() && stream_url.is_empty()
}

pub(super) fn stalker_stream_kind(cluster: XtreamCluster, item_type: PlaylistItemType) -> StalkerStreamKind {
    if item_type == PlaylistItemType::Catchup {
        StalkerStreamKind::Archive
    } else {
        match cluster {
            XtreamCluster::Live => StalkerStreamKind::Live,
            XtreamCluster::Video => StalkerStreamKind::Movie,
            XtreamCluster::Series => StalkerStreamKind::Episode,
        }
    }
}

/// Serialized per account, so aliases of one input resolve independently.
pub(super) async fn re_resolve_stalker_url_singleflight(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    account_name: &str,
    provider_id: u32,
    kind: StalkerStreamKind,
    force_refresh: bool,
) -> Result<Option<Arc<str>>, TuliproxError> {
    let account_id = input.alias_by_name(account_name).map_or(input.id, |alias| alias.id);
    let entry_lock = app_state.stalker_resolve_coordinator.guard_for(account_id, provider_id).await;
    let _flight = entry_lock.lock().await;
    let client = app_state.http_clients.default.load().as_ref().clone();
    re_resolve_stalker_url(&app_state.app_config, &client, input, account_name, provider_id, kind, force_refresh).await
}

/// Resolves a link for the named account; a failure is logged and yields `None`.
/// `force_refresh` bypasses the cached link, for retrying after the provider rejected it.
pub(crate) async fn resolve_stalker_stream_for_account(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    account_name: &Arc<str>,
    provider_id: u32,
    kind: StalkerStreamKind,
    force_refresh: bool,
) -> Option<(Arc<str>, String)> {
    match re_resolve_stalker_url_singleflight(app_state, input, account_name, provider_id, kind, force_refresh).await {
        Ok(url) => url.map(|url| (Arc::clone(account_name), url.to_string())),
        Err(err) => {
            error!(
                "Stalker link resolution failed for account {}: {}",
                sanitize_sensitive_info(account_name),
                sanitize_sensitive_info(&err.to_string())
            );
            None
        }
    }
}

/// Decides whether the requested stream URL is kept for the allocated provider account. A Stalker URL belongs to the
/// account that owns it (the forced provider, else the input); any other account needs its own resolved link.
pub(super) fn accepts_requested_stream_url(
    input: &ConfigInput,
    force_provider: Option<&Arc<str>>,
    accept_requested_stream_url: bool,
    allocated_provider: &str,
) -> bool {
    (accept_requested_stream_url && force_provider.is_none_or(|forced| forced.as_ref() == allocated_provider))
        || (input.input_type.is_stalker()
            && force_provider.map_or(input.name.as_ref(), |forced| forced.as_ref()) == allocated_provider)
}

/// Resolves the first playable link for the input's own account when the playlist holds no Stalker URL yet.
/// Returns `stream_url` unchanged when no resolution is needed or new playback is blocked.
pub(crate) async fn resolve_initial_stalker_playback_url(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    user: &ProxyUserCredentials,
    provider_id: u32,
    cluster: XtreamCluster,
    item_type: PlaylistItemType,
    stream_url: &Arc<str>,
) -> Result<Arc<str>, TuliproxError> {
    // An input in maintenance is not contacted; the maintenance gate further down answers.
    if !needs_initial_stalker_resolution(input.input_type, stream_url)
        || blocks_new_playback(app_state, input, user, false)
    {
        return Ok(Arc::clone(stream_url));
    }
    re_resolve_stalker_url_singleflight(
        app_state,
        input,
        &input.name,
        provider_id,
        stalker_stream_kind(cluster, item_type),
        false,
    )
    .await?
    .ok_or_else(|| {
        TuliproxError::RepositoryStalker(format!(
            "Stalker playback URL could not be resolved for input '{}' and provider id {provider_id}",
            input.name
        ))
    })
}
