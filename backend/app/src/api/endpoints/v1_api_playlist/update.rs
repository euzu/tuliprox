use super::{ManualUpdateEnqueueError, PLAYLIST_UPDATE_STATUS_READ_CONCURRENCY};
use crate::{
    api::{
        auth_middleware::{check_permission, VerifiedClaims},
        model::AppState,
    },
    model::{ProcessTargets, SourcesConfig},
};
use axum::response::IntoResponse;
use log::{debug, error};
use serde_json::json;
use shared::{
    error::TuliproxError,
    model::{
        permission::Permission, InputPlaylistUpdateStatusDto, InputType, InputUpdateAction, InputUpdateCapabilities,
        InputUpdateRequest, OperationRunAccepted, PersistedPlaylistUpdateClusterState,
        PersistedPlaylistUpdateClusterStatusDto, PersistedPlaylistUpdateInputResult, PlaylistUpdateRequestDto,
        PlaylistUpdateRequestPayload, PlaylistUpdateRunId, PlaylistUpdateState, PlaylistUpdateStatusDto, XtreamCluster,
    },
    utils::sanitize_sensitive_info,
};
use std::sync::Arc;

fn enqueue_manual_playlist_update(
    sender: &tokio::sync::mpsc::Sender<crate::api::model::ManualPlaylistUpdateRequest>,
    targets: Arc<ProcessTargets>,
    input_action: Option<InputUpdateRequest>,
) -> Result<PlaylistUpdateRunId, ManualUpdateEnqueueError> {
    let permit = sender.try_reserve().map_err(|error| match error {
        tokio::sync::mpsc::error::TrySendError::Full(()) => ManualUpdateEnqueueError::Busy,
        tokio::sync::mpsc::error::TrySendError::Closed(()) => ManualUpdateEnqueueError::Unavailable,
    })?;
    let run_id = PlaylistUpdateRunId::generate();
    permit.send(crate::api::model::ManualPlaylistUpdateRequest { run_id: run_id.clone(), targets, input_action });
    Ok(run_id)
}

pub(super) async fn playlist_update(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    claims: Option<axum::Extension<VerifiedClaims>>,
    axum::extract::Json(payload): axum::extract::Json<PlaylistUpdateRequestPayload>,
) -> impl axum::response::IntoResponse + Send {
    let request = payload.into_request();
    let input_action = match request.manual_input_update() {
        Ok(action) => action,
        Err(error) => {
            return (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": error}))).into_response()
        }
    };
    if input_action.is_some_and(|request| request.action == InputUpdateAction::Rescan) {
        let config = app_state.app_config.config.load();
        if !config.library.as_ref().is_some_and(|library| library.enabled) {
            return (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": "Library is not enabled"})))
                .into_response();
        }
        if config.web_ui.as_ref().and_then(|web| web.auth.as_ref()).is_some() {
            let Some(axum::Extension(VerifiedClaims(claims))) = claims else {
                return axum::http::StatusCode::UNAUTHORIZED.into_response();
            };
            if check_permission::<{ Permission::LibraryWrite as u32 }>(&app_state, &claims, None).is_err() {
                return axum::http::StatusCode::FORBIDDEN.into_response();
            }
        }
    }
    let process_targets = resolve_manual_playlist_update_targets(&app_state.app_config.sources.load(), &request);
    match process_targets {
        Ok(valid_targets) => {
            let valid_targets = Arc::new(valid_targets);
            match enqueue_manual_playlist_update(
                &app_state.playlist_updates.manual_update_sender,
                valid_targets,
                input_action,
            ) {
                Ok(run_id) => {
                    (axum::http::StatusCode::ACCEPTED, axum::Json(OperationRunAccepted::playlist_update(run_id)))
                        .into_response()
                }
                Err(ManualUpdateEnqueueError::Busy) => {
                    debug!("Manual playlist update rejected: another update is already queued");
                    (
                        axum::http::StatusCode::CONFLICT,
                        axum::Json(json!({"error": "Another playlist update is already queued; retry this request"})),
                    )
                        .into_response()
                }
                Err(ManualUpdateEnqueueError::Unavailable) => {
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

pub(super) fn resolve_manual_playlist_update_targets(
    sources: &SourcesConfig,
    request: &PlaylistUpdateRequestDto,
) -> Result<ProcessTargets, TuliproxError> {
    let manual_action =
        request.manual_input_update().map_err(|error| TuliproxError::ConfigSource(error.to_string()))?;
    let manual_input = if let Some(manual) = manual_action {
        let input = sources
            .inputs
            .iter()
            .find(|input| input.id == manual.input_id)
            .ok_or_else(|| TuliproxError::ConfigSource(format!("No input found for id {}", manual.input_id)))?;
        if !InputUpdateCapabilities::for_input_type(input.input_type, input.enabled).supports_action(manual.action) {
            return Err(TuliproxError::ConfigSource("Manual action is not supported for this input".to_string()));
        }
        // Only the additive action wire form requires IDs. Legacy input_refresh
        // requests retain the existing name resolution, including empty = all.
        if request.input_action.is_some() && request.target_ids.is_none() {
            return Err(TuliproxError::ConfigSource("Manual input actions require explicit target IDs".to_string()));
        }
        Some(input)
    } else {
        None
    };
    let Some(target_ids) = request.target_ids.as_deref() else {
        let targets = (!request.targets.is_empty()).then_some(&request.targets);
        return sources.validate_targets(targets);
    };
    if !request.targets.is_empty() {
        return Err(TuliproxError::ConfigSource(
            "Manual playlist update cannot contain both target names and target IDs".to_string(),
        ));
    }
    if target_ids.is_empty() {
        return Err(TuliproxError::ConfigSource(
            "Manual playlist update target ID selection must not be empty".to_string(),
        ));
    }

    let mut targets = Vec::with_capacity(target_ids.len());
    let mut target_names = Vec::with_capacity(target_ids.len());
    for target_id in target_ids {
        let Some(target) = sources.get_target_by_id(*target_id) else {
            return Err(TuliproxError::ConfigSource(format!("No target found for id {target_id}")));
        };
        if manual_input.is_some_and(|input| {
            !sources.sources.iter().any(|source| {
                source.inputs.contains(&input.name) && source.targets.iter().any(|target| target.id == *target_id)
            })
        }) {
            return Err(TuliproxError::ConfigSource("Target does not belong to the selected input".to_string()));
        }
        targets.push(target.id);
        target_names.push(target.name.clone());
    }

    Ok(ProcessTargets {
        enabled: true,
        inputs: sources.inputs.iter().map(|input| input.id).collect(),
        targets,
        target_names,
    })
}

pub(super) async fn read_playlist_update_input_status(
    read: impl FnOnce() -> crate::processing::input_cache::InputStatus + Send + 'static,
) -> Result<crate::processing::input_cache::InputStatus, axum::http::StatusCode> {
    tokio::task::spawn_blocking(read).await.map_err(|error| {
        error!("Playlist update status reader failed: {error}");
        axum::http::StatusCode::INTERNAL_SERVER_ERROR
    })
}

pub(super) async fn playlist_update_status(
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> Result<axum::Json<PlaylistUpdateStatusDto>, axum::http::StatusCode> {
    let storage_dir = app_state.app_config.config.load().storage_dir.clone();
    let inputs = app_state.app_config.sources.load().inputs.clone();
    let status_reads = futures::stream::iter(inputs.into_iter().map(|input| {
        let storage_dir = storage_dir.clone();
        let input_name = input.name.clone();
        async move {
            let storage_path =
                crate::processing::input_cache::resolve_input_storage_path(&storage_dir, &input_name).await;
            let input_status = read_playlist_update_input_status(move || {
                crate::processing::input_cache::load_input_status(&storage_path)
            })
            .await?;
            Ok::<_, axum::http::StatusCode>((input, input_status))
        }
    }));
    let status_reads = {
        use futures::StreamExt as _;
        status_reads.buffered(PLAYLIST_UPDATE_STATUS_READ_CONCURRENCY)
    };
    let input_statuses: Vec<_> = futures::StreamExt::collect(status_reads).await;
    let input_statuses = input_statuses.into_iter().collect::<Result<Vec<_>, _>>()?;
    let mut statuses = Vec::with_capacity(input_statuses.len());
    for (input, input_status) in input_statuses {
        let cluster_based = input.input_type.is_xtream() || input.input_type.is_stalker();
        let last_update =
            input_status.clusters.values().map(|cluster| cluster.timestamp).filter(|timestamp| *timestamp > 0).max();
        let clusters = if cluster_based || input.input_type == InputType::M3u {
            [XtreamCluster::Live, XtreamCluster::Video, XtreamCluster::Series]
                .into_iter()
                .filter_map(|cluster| {
                    input_status.clusters.get(cluster.as_ref()).map(|persisted| {
                        PersistedPlaylistUpdateClusterStatusDto {
                            cluster,
                            status: match &persisted.status {
                                crate::processing::input_cache::ClusterState::Ok => {
                                    PersistedPlaylistUpdateClusterState::Ok
                                }
                                crate::processing::input_cache::ClusterState::Failed => {
                                    PersistedPlaylistUpdateClusterState::Failed
                                }
                            },
                            timestamp: persisted.timestamp,
                            last_update: persisted.last_update,
                        }
                    })
                })
                .collect()
        } else {
            Vec::new()
        };
        let last_input_update = input_status.last_input_update.or_else(|| {
            // Legacy cluster-free inputs already persisted a general default status.
            // Do not infer an aggregate input/target or Quality result from clusters.
            if cluster_based {
                return None;
            }
            input_status.clusters.get("default").filter(|status| status.timestamp > 0).map(|status| {
                PersistedPlaylistUpdateInputResult {
                    state: match status.status {
                        crate::processing::input_cache::ClusterState::Ok => PlaylistUpdateState::Success,
                        crate::processing::input_cache::ClusterState::Failed => PlaylistUpdateState::Failure,
                    },
                    timestamp: status.timestamp,
                }
            })
        });
        statuses.push(InputPlaylistUpdateStatusDto { input_id: input.id, last_update, last_input_update, clusters });
    }
    let active_updates = app_state
        .event_manager
        .playlist_update_snapshot()
        .into_iter()
        .filter(|event| event.input_id.is_some_and(|id| statuses.iter().any(|status| status.input_id == id)))
        .collect();
    Ok(axum::Json(PlaylistUpdateStatusDto { inputs: statuses, active_updates }))
}
