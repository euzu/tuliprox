//! Write routes of the failover group editor: save, reset and reassign a target's override section,
//! and start a playlist update for one target.

use super::{
    enabled_target, error_response, json_error, path_string, respond, respond_with, ApiError, ApiResult, TargetQuery,
};
use crate::api::{
    api_utils::failover_group_hot_apply::try_hot_apply,
    endpoints::v1_api_playlist::{
        enqueue_manual_playlist_update, resolve_manual_playlist_update_targets, ManualUpdateEnqueueError,
        QUEUE_BUSY_MESSAGE,
    },
    model::{AppState, EventMessage},
};
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::Response,
    routing::{post, put},
    Json, Router,
};
use log::error;
use shared::model::{
    FailoverGroupConflictDto, FailoverGroupSaveResultDto, OperationRunAccepted, PlaylistUpdateRequestDto,
    ReassignOverridesRequestDto, SaveOverridesRequestDto,
};
use std::sync::Arc;
use tuliprox_core::model::TargetOverridesEntry;
use tuliprox_repository::{reassign_failover_section, write_failover_section, FailoverWriteError};

pub(super) fn write_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/failover/overrides", put(put_overrides).delete(delete_overrides))
        .route("/failover/overrides/reassign", post(post_reassign))
        .route("/failover/apply", post(post_apply))
}

#[derive(serde::Deserialize)]
struct DeleteQuery {
    target: String,
    expected_etag: String,
}

impl From<FailoverWriteError> for ApiError {
    fn from(err: FailoverWriteError) -> Self {
        match err {
            FailoverWriteError::Conflict { current_etag } => {
                json_error(StatusCode::CONFLICT, &FailoverGroupConflictDto { current_etag })
            }
            FailoverWriteError::Invalid(findings) => json_error(StatusCode::UNPROCESSABLE_ENTITY, &findings),
            FailoverWriteError::Locked(message) => error_response(StatusCode::LOCKED, message),
            FailoverWriteError::Io(err) => {
                error!("Failed to write failover overrides: {err}");
                error_response(StatusCode::INTERNAL_SERVER_ERROR, err)
            }
        }
    }
}

fn notify_changed(state: &AppState, targets: Vec<String>) {
    state.event_manager.send_event(EventMessage::FailoverOverridesChanged { targets });
}

fn save_result(entry: Option<&TargetOverridesEntry>, hot_applied: bool) -> FailoverGroupSaveResultDto {
    FailoverGroupSaveResultDto {
        etag: entry.map(|entry| entry.etag.to_string()),
        file: entry.map(|entry| path_string(&entry.file)),
        hot_applied,
    }
}

async fn put_overrides(
    State(state): State<Arc<AppState>>,
    Query(query): Query<TargetQuery>,
    Json(request): Json<SaveOverridesRequestDto>,
) -> Response {
    respond(save_overrides(&state, &query.target, request).await)
}

/// Writes the section (an empty one removes it) and hot applies it when only order or policy changed.
async fn save_overrides(
    state: &AppState,
    target_name: &str,
    request: SaveOverridesRequestDto,
) -> ApiResult<FailoverGroupSaveResultDto> {
    let (target, _) = enabled_target(state, target_name)?;
    let SaveOverridesRequestDto { expected_etag, overrides } = request;
    let entry =
        write_failover_section(&state.app_config, &target.name, expected_etag.as_deref(), Some(overrides)).await?;
    let hot_applied = match &entry {
        Some(entry) => try_hot_apply(state, &target.name, entry).await,
        None => false,
    };
    notify_changed(state, vec![target.name.clone()]);
    Ok(save_result(entry.as_deref(), hot_applied))
}

async fn delete_overrides(State(state): State<Arc<AppState>>, Query(query): Query<DeleteQuery>) -> Response {
    respond(reset_overrides(&state, query).await)
}

/// Removes the whole section of a target, also one whose target no longer exists.
async fn reset_overrides(state: &AppState, query: DeleteQuery) -> ApiResult<FailoverGroupSaveResultDto> {
    write_failover_section(&state.app_config, &query.target, Some(&query.expected_etag), None).await?;
    notify_changed(state, vec![query.target]);
    Ok(save_result(None, false))
}

async fn post_reassign(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ReassignOverridesRequestDto>,
) -> Response {
    respond(reassign_overrides(&state, request).await)
}

/// Moves the section of `from` (typically an orphan) to the target `to`, which must have failover groups enabled.
async fn reassign_overrides(
    state: &AppState,
    request: ReassignOverridesRequestDto,
) -> ApiResult<FailoverGroupSaveResultDto> {
    let ReassignOverridesRequestDto { from, to, expected_etag } = request;
    enabled_target(state, &to)?;
    let entry = reassign_failover_section(&state.app_config, &from, &to, &expected_etag).await?;
    notify_changed(state, vec![from, to]);
    Ok(save_result(Some(&entry), false))
}

async fn post_apply(State(state): State<Arc<AppState>>, Query(query): Query<TargetQuery>) -> Response {
    respond_with(StatusCode::ACCEPTED, apply_response(&state, &query.target))
}

/// Queues a manual playlist update (default policy) for exactly this target.
fn apply_response(state: &AppState, target_name: &str) -> ApiResult<OperationRunAccepted> {
    let (target, _) = enabled_target(state, target_name)?;
    let request = PlaylistUpdateRequestDto { targets: vec![target.name.clone()], ..Default::default() };
    let targets = resolve_manual_playlist_update_targets(&state.app_config.sources.load(), &request)
        .map_err(|err| error_response(StatusCode::BAD_REQUEST, err))?;
    match enqueue_manual_playlist_update(&state.playlist_updates.manual_update_sender, Arc::new(targets), None) {
        Ok(run_id) => Ok(OperationRunAccepted::playlist_update(run_id)),
        Err(ManualUpdateEnqueueError::Busy) => Err(error_response(StatusCode::CONFLICT, QUEUE_BUSY_MESSAGE)),
        Err(ManualUpdateEnqueueError::Unavailable) => {
            Err(error_response(StatusCode::SERVICE_UNAVAILABLE, "Playlist update worker is not available"))
        }
    }
}
