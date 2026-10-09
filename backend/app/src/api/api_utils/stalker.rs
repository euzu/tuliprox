use crate::{api::model::AppState, model::ConfigInput, processing::processor::re_resolve_stalker_url};
use axum::http::StatusCode;
use shared::{
    error::TuliproxError,
    model::{InputType, PlaylistItemType, StalkerStreamKind, XtreamCluster},
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

pub(super) async fn re_resolve_stalker_url_singleflight(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    provider_id: u32,
    kind: StalkerStreamKind,
    force_refresh: bool,
) -> Result<Option<Arc<str>>, TuliproxError> {
    let entry_lock = app_state.stalker_resolve_coordinator.guard_for(input.id, provider_id).await;
    let _flight = entry_lock.lock().await;
    let client = app_state.http_clients.default.load().as_ref().clone();
    re_resolve_stalker_url(&app_state.app_config, &client, input, provider_id, kind, force_refresh).await
}

pub(crate) async fn resolve_initial_stalker_playback_url(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    provider_id: u32,
    cluster: XtreamCluster,
    item_type: PlaylistItemType,
    stream_url: &Arc<str>,
) -> Result<Arc<str>, TuliproxError> {
    if !needs_initial_stalker_resolution(input.input_type, stream_url) {
        return Ok(Arc::clone(stream_url));
    }
    re_resolve_stalker_url_singleflight(app_state, input, provider_id, stalker_stream_kind(cluster, item_type), false)
        .await?
        .ok_or_else(|| {
            TuliproxError::RepositoryStalker(format!(
                "Stalker playback URL could not be resolved for input '{}' and provider id {provider_id}",
                input.name
            ))
        })
}
