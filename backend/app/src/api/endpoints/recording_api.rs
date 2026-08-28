//! Recording REST routes.

use crate::{
    api::{
        endpoints::recording_media_api::AuthClaims,
        model::{
            event_manager::EventMessage,
            mutate, recording_quota,
            recording_rule_service::{DeleteFuture, RuleServiceError},
            recording_service::{
                CreateRecordingInput, EditRecordingPatch, RecordingService, RecordingSourceInput, ServiceError,
            },
            recording_ws, AppState, DownloadQueue, FileDownload,
        },
    },
    repository::recording_rule_repository::RecordingRuleRepository,
};
use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, patch, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use shared::model::{
    recording::{RecordingMetadata, RecordingOwner, RecordingProvenance, RecordingSource, RecordingVisibility},
    recording_rule::{RecordingRule, RuleBody, RuleSource, RuleVisibility},
    FileDownloadDto, Permission, RecordingTypeDto, UserId, XtreamCluster, ROLE_ADMIN,
};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: &'static str,
}

fn error_response(status: StatusCode, error: &'static str) -> axum::response::Response {
    (status, Json(ErrorResponse { error })).into_response()
}

fn service_error_response(err: &ServiceError) -> axum::response::Response {
    error_response(service_error_status(err), err.code())
}

/// HTTP status for a service error. The wire code always comes from
/// `ServiceError::code`, so it is not duplicated here.
fn service_error_status(err: &ServiceError) -> StatusCode {
    match err {
        ServiceError::UnknownOwner => StatusCode::UNAUTHORIZED,
        ServiceError::InvalidSource
        | ServiceError::InvalidInterval
        | ServiceError::PaddingLimitExceeded
        | ServiceError::InvalidState
        | ServiceError::ProvenanceImmutable
        | ServiceError::Duplicate
        | ServiceError::InvalidPath
        | ServiceError::QuotaExceeded => StatusCode::BAD_REQUEST,
        ServiceError::DiskFull => StatusCode::INSUFFICIENT_STORAGE,
        ServiceError::Disabled => StatusCode::NOT_IMPLEMENTED,
        ServiceError::Forbidden | ServiceError::SharedCreationNotAdministrator => StatusCode::FORBIDDEN,
        ServiceError::UnknownRecording => StatusCode::NOT_FOUND,
        ServiceError::PersistenceFailed | ServiceError::IoError(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// GET /api/v1/recording/tasks
pub async fn list_recording_tasks(
    axum::extract::Query(params): axum::extract::Query<ListTasksParams>,
    State(app_state): State<Arc<AppState>>,
    AuthClaims(claims): AuthClaims,
) -> impl IntoResponse {
    if !claims.permissions.contains(Permission::RecordingRead) {
        return error_response(StatusCode::FORBIDDEN, "recording_forbidden");
    }
    // Filtering by an arbitrary owner is an administrator capability.
    // The visibility filter below already prevents a regular user from
    // *seeing* another owner's private tasks, but accepting the
    // parameter and silently returning an empty list made the API read
    // as if cross-owner queries were supported. Reject it explicitly.
    if params.owner.is_some() && !is_admin(&claims) {
        return error_response(StatusCode::FORBIDDEN, "recording_forbidden");
    }
    let (revision, mut tasks) = recording_ws::recording_snapshot(&app_state.downloads, &claims).await;
    if let Some(owner) = params.owner.as_deref() {
        tasks.retain(|task| {
            task.recording.as_ref().and_then(|recording| recording.owner_id.as_ref()).is_some_and(|id| id.0 == owner)
        });
    }
    if let Some(visibility) = params.visibility.as_deref() {
        tasks.retain(|task| {
            task.recording.as_ref().is_some_and(|recording| {
                matches!(
                    (visibility, recording.visibility),
                    ("private", RecordingVisibility::Private) | ("shared", RecordingVisibility::Shared)
                )
            })
        });
    }
    Json(RecordingSnapshotResponse { revision: revision.0, tasks }).into_response()
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListTasksParams {
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub visibility: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecordingSnapshotResponse {
    pub revision: u64,
    pub tasks: Vec<FileDownloadDto>,
}

/// POST /api/v1/recording/tasks
pub async fn create_recording_task(
    State(app_state): State<Arc<AppState>>,
    AuthClaims(claims): AuthClaims,
    Json(body): Json<CreateRecordingTaskBody>,
) -> impl IntoResponse {
    let mut source = body.source.clone();
    let Some(resolved_source) = resolve_recording_source(
        &app_state,
        &source.target_id,
        &mut source.virtual_id,
        &mut source.input_name,
        source.cluster,
    )
    .await
    else {
        return service_error_response(&ServiceError::InvalidSource);
    };
    if source.cluster != XtreamCluster::Live {
        return create_http_recording_task(&app_state, &claims, body, source, resolved_source).await;
    }
    let (Some(program_start), Some(program_end)) = (body.program_start, body.program_end) else {
        return error_response(StatusCode::UNPROCESSABLE_ENTITY, "recording_invalid_interval");
    };
    let service = RecordingService::new(app_state.downloads.clone(), app_state.app_config.clone());
    let input = CreateRecordingInput {
        source: RecordingSourceInput {
            target_id: source.target_id,
            virtual_id: source.virtual_id,
            cluster: source.cluster,
            input_name: source.input_name,
        },
        program_title: body.program_title,
        program_start,
        program_end,
        pre_roll_secs: body.pre_roll_secs.unwrap_or_default(),
        post_roll_secs: body.post_roll_secs.unwrap_or_default(),
        visibility: body.visibility,
        channel_id: body.channel_id,
        channel_name: body.channel_name,
        provenance: RecordingProvenance::default(),
        epg: body.epg,
    };
    match service.create_recording(&claims, &input).await {
        Ok(view) => {
            let _ = app_state.event_manager.send_event(EventMessage::RecordingChanged);
            Json(CreateRecordingTaskResponse { id: view.uuid, title: view.filename_preview, recording: None })
                .into_response()
        }
        Err(err) => service_error_response(&err),
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateRecordingTaskBody {
    pub source: CreateRecordingSourceBody,
    pub program_title: String,
    #[serde(default)]
    pub program_start: Option<i64>,
    #[serde(default)]
    pub program_end: Option<i64>,
    #[serde(default)]
    pub pre_roll_secs: Option<u64>,
    #[serde(default)]
    pub post_roll_secs: Option<u64>,
    pub visibility: RecordingVisibility,
    #[serde(default)]
    pub channel_id: Option<String>,
    #[serde(default)]
    pub channel_name: Option<String>,
    #[serde(default)]
    pub epg: Option<shared::model::recording::EpgEpisodeMetadata>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateRecordingSourceBody {
    pub target_id: String,
    pub virtual_id: String,
    pub cluster: XtreamCluster,
    pub input_name: String,
}

async fn create_http_recording_task(
    app_state: &Arc<AppState>,
    claims: &shared::model::Claims,
    body: CreateRecordingTaskBody,
    source: CreateRecordingSourceBody,
    resolved: crate::api::endpoints::v1_api_playlist::ResolvedRecordingSource,
) -> axum::response::Response {
    if body.program_start.is_some()
        || body.program_end.is_some()
        || body.pre_roll_secs.is_some()
        || body.post_roll_secs.is_some()
    {
        return error_response(StatusCode::UNPROCESSABLE_ENTITY, "recording_invalid_interval");
    }
    if !claims.permissions.contains(Permission::RecordingWrite) {
        return error_response(StatusCode::FORBIDDEN, "recording_forbidden");
    }
    let Some(owner_id) = claims.subject_id.clone() else {
        return error_response(StatusCode::UNAUTHORIZED, "recording_token_refresh_required");
    };
    if body.visibility == RecordingVisibility::Shared && !is_admin(claims) {
        return error_response(StatusCode::FORBIDDEN, "recording_shared_requires_administrator");
    }
    let recording_type = match source.cluster {
        XtreamCluster::Video => RecordingTypeDto::Vod,
        XtreamCluster::Series => RecordingTypeDto::Series,
        XtreamCluster::Live => return error_response(StatusCode::BAD_REQUEST, "recording_invalid_source"),
    };
    if !resolved.downloadable {
        return error_response(StatusCode::UNPROCESSABLE_ENTITY, "recording_invalid_source");
    }
    let Some(extension) = resolved.extension.as_deref().filter(|extension| !extension.is_empty()) else {
        return error_response(StatusCode::UNPROCESSABLE_ENTITY, "recording_invalid_path");
    };
    let Some(url) = crate::api::endpoints::v1_api_playlist::build_stable_recording_url(
        &app_state.app_config,
        &source.target_id,
        &source.input_name,
        resolved.virtual_id,
        source.cluster,
    ) else {
        return error_response(StatusCode::BAD_REQUEST, "recording_invalid_source");
    };
    let config = app_state.app_config.config.load();
    let Some(recording_config) = config.recording.as_ref() else {
        return error_response(StatusCode::NOT_IMPLEMENTED, "recording_disabled");
    };
    let filename = format!("{}.{}", resolved.title, extension.trim_start_matches('.'));
    let Some(mut task) = FileDownload::new_with_type(
        &url,
        &filename,
        recording_config,
        Some(Arc::from(source.input_name.as_str())),
        recording_config.priority,
        recording_type,
    ) else {
        return error_response(StatusCode::UNPROCESSABLE_ENTITY, "recording_invalid_path");
    };
    let recording_source =
        RecordingSource::new(source.target_id, source.virtual_id, source.input_name).with_cluster(source.cluster);
    let mut metadata = RecordingMetadata::new_transfer(
        RecordingOwner::User(owner_id),
        body.visibility,
        recording_source,
        resolved.title,
    );
    metadata.relative_path = task
        .file_path
        .strip_prefix(&recording_config.directory)
        .ok()
        .and_then(|path| path.to_str())
        .map(str::to_string);
    task.recording = Some(metadata);

    if let Some(existing) = app_state.downloads.find_duplicate(&task).await {
        return Json(FileDownloadDto::from(&existing)).into_response();
    }
    if let Err(error) = mutate(&app_state.downloads, |candidate| {
        candidate.queue.push(DownloadQueue::to_persisted(&task));
        Ok(())
    })
    .await
    {
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, error.message());
    }
    if app_state.downloads.active.read().await.is_none() {
        if crate::api::endpoints::download_api::ensure_download_worker_running(
            &app_state.app_config,
            recording_config,
            &app_state.downloads,
            &app_state.event_manager,
            &app_state.active_provider,
            &app_state.connection_manager,
        )
        .await
        .is_err()
        {
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "recording_worker_failed");
        }
    }
    let _ = app_state.event_manager.send_event(EventMessage::RecordingChanged);
    Json(FileDownloadDto::from(&task)).into_response()
}

#[derive(Debug, Clone, Serialize)]
pub struct CreateRecordingTaskResponse {
    pub id: String,
    pub title: String,
    pub recording: Option<shared::model::recording::RecordingTaskDto>,
}

/// PATCH /api/v1/recording/tasks/{id}
pub async fn edit_recording_task(
    axum::extract::Path(id): axum::extract::Path<String>,
    State(app_state): State<Arc<AppState>>,
    AuthClaims(claims): AuthClaims,
    Json(body): Json<EditRecordingTaskBody>,
) -> impl IntoResponse {
    let service = RecordingService::new(app_state.downloads.clone(), app_state.app_config.clone());
    match service.edit_recording(&claims, &id, body.into()).await {
        Ok(_view) => {
            let _ = app_state.event_manager.send_event(EventMessage::RecordingChanged);
            StatusCode::NO_CONTENT.into_response()
        }
        Err(err) => service_error_response(&err),
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct EditRecordingTaskBody {
    #[serde(default)]
    pub program_start: Option<i64>,
    #[serde(default)]
    pub program_end: Option<i64>,
    #[serde(default)]
    pub pre_roll_secs: Option<u64>,
    #[serde(default)]
    pub post_roll_secs: Option<u64>,
    #[serde(default)]
    pub program_title: Option<String>,
    #[serde(default)]
    pub channel_id: Option<String>,
    #[serde(default)]
    pub channel_name: Option<String>,
}

impl From<EditRecordingTaskBody> for EditRecordingPatch {
    fn from(value: EditRecordingTaskBody) -> Self {
        Self {
            program_start: value.program_start,
            program_end: value.program_end,
            pre_roll_secs: value.pre_roll_secs,
            post_roll_secs: value.post_roll_secs,
            program_title: value.program_title,
            channel_id: value.channel_id,
            channel_name: value.channel_name,
        }
    }
}

/// POST /api/v1/recording/tasks/{id}/cancel

/// POST /api/v1/recording/tasks/{id}/pause
pub async fn pause_recording_task(
    axum::extract::Path(id): axum::extract::Path<String>,
    State(app_state): State<Arc<AppState>>,
    AuthClaims(claims): AuthClaims,
) -> impl IntoResponse {
    let service = RecordingService::new(app_state.downloads.clone(), app_state.app_config.clone());
    match service.pause_recording(&claims, &id).await {
        Ok(()) => {
            let _ = app_state.event_manager.send_event(EventMessage::RecordingChanged);
            StatusCode::NO_CONTENT.into_response()
        }
        Err(err) => service_error_response(&err),
    }
}

/// POST /api/v1/recording/tasks/{id}/resume
pub async fn resume_recording_task(
    axum::extract::Path(id): axum::extract::Path<String>,
    State(app_state): State<Arc<AppState>>,
    AuthClaims(claims): AuthClaims,
) -> impl IntoResponse {
    let service = RecordingService::new(app_state.downloads.clone(), app_state.app_config.clone());
    match service.resume_recording(&claims, &id).await {
        Ok(_) => {
            let _ = app_state.event_manager.send_event(EventMessage::RecordingChanged);
            StatusCode::NO_CONTENT.into_response()
        }
        Err(err) => service_error_response(&err),
    }
}

/// POST /api/v1/recording/tasks/{id}/retry
pub async fn retry_recording_task(
    axum::extract::Path(id): axum::extract::Path<String>,
    State(app_state): State<Arc<AppState>>,
    AuthClaims(claims): AuthClaims,
) -> impl IntoResponse {
    let service = RecordingService::new(app_state.downloads.clone(), app_state.app_config.clone());
    match service.retry_recording(&claims, &id).await {
        Ok(_) => {
            let _ = app_state.event_manager.send_event(EventMessage::RecordingChanged);
            StatusCode::NO_CONTENT.into_response()
        }
        Err(err) => service_error_response(&err),
    }
}

/// DELETE /api/v1/recording/tasks/{id}
pub async fn remove_recording_task(
    axum::extract::Path(id): axum::extract::Path<String>,
    State(app_state): State<Arc<AppState>>,
    AuthClaims(claims): AuthClaims,
) -> impl IntoResponse {
    let service = RecordingService::new(app_state.downloads.clone(), app_state.app_config.clone());
    match service.remove_recording_task(&claims, &id).await {
        Ok(_) => {
            let _ = app_state.event_manager.send_event(EventMessage::RecordingChanged);
            StatusCode::NO_CONTENT.into_response()
        }
        Err(err) => service_error_response(&err),
    }
}

pub async fn cancel_recording_task(
    axum::extract::Path(id): axum::extract::Path<String>,
    State(app_state): State<Arc<AppState>>,
    AuthClaims(claims): AuthClaims,
) -> impl IntoResponse {
    let service = RecordingService::new(app_state.downloads.clone(), app_state.app_config.clone());
    match service.cancel_recording(&claims, &id).await {
        Ok(()) => {
            let _ = app_state.event_manager.send_event(EventMessage::RecordingChanged);
            StatusCode::NO_CONTENT.into_response()
        }
        Err(err) => service_error_response(&err),
    }
}

/// DELETE /api/v1/recording/tasks/{id}
pub async fn delete_recording_task(
    axum::extract::Path(id): axum::extract::Path<String>,
    State(app_state): State<Arc<AppState>>,
    AuthClaims(claims): AuthClaims,
) -> impl IntoResponse {
    let service = RecordingService::new(app_state.downloads.clone(), app_state.app_config.clone());
    match service.delete_recording(&claims, &id).await {
        Ok(()) => {
            let _ = app_state.event_manager.send_event(EventMessage::RecordingChanged);
            StatusCode::NO_CONTENT.into_response()
        }
        Err(err) => service_error_response(&err),
    }
}

/// POST /api/v1/recording/conflicts/preview
///
/// Errors use the same `{"error": "<code>"}` envelope as every other
/// recording route. It used to return a bare status/string pair, so a
/// client could not map a preview failure through the shared error
/// handling the rest of the API uses.
pub async fn preview_recording_conflicts(
    AuthClaims(claims): AuthClaims,
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(body): axum::extract::Json<PreviewConflictsBody>,
) -> axum::response::Response {
    let source = RecordingSourceInput::from(&body.source);
    let request = crate::api::model::recording_service::ConflictPreviewRequest {
        source,
        padded_start: body.candidate.padded_start,
        padded_end: body.candidate.padded_end,
        pre_roll_secs: body.candidate.pre_roll_secs,
        post_roll_secs: body.candidate.post_roll_secs,
        priority: body.candidate.priority,
    };
    let service = crate::api::model::recording_service::RecordingService::from_ctx(&state.recording_ctx());
    let preview = match service.preview_conflicts(&claims, &request).await {
        Ok(preview) => preview,
        Err(err) => return service_error_response(&err),
    };
    Json(PreviewConflictsResponse {
        severity: preview.severity.as_wire().to_string(),
        provider_scope: preview.provider_scope,
        overlap_segments: preview
            .overlap_segments
            .into_iter()
            .map(|s| OverlapSegmentDto { start: s.start, end: s.end, peak_demand: s.peak_demand })
            .collect(),
    })
    .into_response()
}

#[derive(Debug, Clone, Deserialize)]
pub struct PreviewConflictsBody {
    /// Server-owned source identifiers. The caller never submits
    /// another recording's padded interval, capacity, or provider
    /// identifier — those are derived server-side.
    pub source: PreviewSourceDto,
    pub candidate: PreviewCandidateDto,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PreviewSourceDto {
    pub target_name: String,
    pub virtual_id: String,
    pub input_name: String,
}

impl From<&PreviewSourceDto> for RecordingSourceInput {
    /// `PreviewConflictsBody::source` is the only DTO that carries a
    /// source across the preview endpoint, so the field mapping lives
    /// here rather than at the call site: adding a field to
    /// `RecordingSourceInput` then fails the compile in exactly one
    /// place, and the cluster choice stays consistent with whatever the
    /// preview service decides to support in the future.
    ///
    /// `XtreamCluster::Live` is the only cluster the preview surface
    /// currently accepts; widening it is a deliberate, single-site
    /// change rather than a quiet drift in the handler.
    fn from(value: &PreviewSourceDto) -> Self {
        Self {
            target_id: value.target_name.clone(),
            virtual_id: value.virtual_id.clone(),
            cluster: shared::model::XtreamCluster::Live,
            input_name: value.input_name.clone(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct PreviewCandidateDto {
    pub padded_start: i64,
    pub padded_end: i64,
    #[serde(default)]
    pub pre_roll_secs: u64,
    #[serde(default)]
    pub post_roll_secs: u64,
    #[serde(default)]
    pub priority: i32,
}

#[derive(Debug, Clone, Serialize)]
pub struct PreviewConflictsResponse {
    pub severity: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_scope: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub overlap_segments: Vec<OverlapSegmentDto>,
}

#[derive(Debug, Clone, Serialize)]
pub struct OverlapSegmentDto {
    pub start: i64,
    pub end: i64,
    pub peak_demand: u32,
}

/// GET /api/v1/recording/quota
pub async fn get_recording_quota(
    State(app_state): State<Arc<AppState>>,
    AuthClaims(claims): AuthClaims,
) -> impl IntoResponse {
    if !claims.permissions.contains(Permission::RecordingRead) {
        return error_response(StatusCode::FORBIDDEN, "recording_forbidden");
    }
    let Some(subject_id) = claims.subject_id.as_ref() else {
        return error_response(StatusCode::UNAUTHORIZED, "recording_token_refresh_required");
    };
    let tasks = all_recording_tasks(&app_state).await;
    let totals = recording_quota::compute_totals(&tasks);
    let config = app_state.app_config.config.load();
    let limits = quota_limits_from_config(config.recording.as_ref().and_then(|recording| recording.quota.as_ref()));
    let quota = recording_quota::regular_user_dto(subject_id, &totals, &limits, &tasks);
    Json(RecordingQuotaResponse {
        private_used_bytes: quota.private.measured_bytes.saturating_add(quota.private.reserved_bytes),
        private_limit_bytes: quota.private.limit_bytes,
        shared_used_bytes: quota.shared.used_bytes,
        shared_limit_bytes: quota.shared.limit_bytes,
        revision: app_state.downloads.revision.load(std::sync::atomic::Ordering::SeqCst),
    })
    .into_response()
}

#[derive(Debug, Clone, Serialize)]
pub struct RecordingQuotaResponse {
    pub private_used_bytes: u64,
    pub private_limit_bytes: Option<u64>,
    pub shared_used_bytes: u64,
    pub shared_limit_bytes: Option<u64>,
    pub revision: u64,
}

/// GET /api/v1/recording/health
///
/// Liveness of the DVR supervisors. Administrator-only: the tick
/// timestamps and the outbox depth describe server internals, not the
/// caller's recordings.
///
/// A supervisor whose `last_*` field is `null` has never completed a
/// pass. Combined with the configured interval, an operator can tell a
/// healthy supervisor from one that died without reading the log.
pub async fn get_recording_health(
    State(app_state): State<Arc<AppState>>,
    AuthClaims(claims): AuthClaims,
) -> impl IntoResponse {
    if !is_admin(&claims) {
        return error_response(StatusCode::FORBIDDEN, "recording_forbidden");
    }
    let health = crate::api::model::recording::recording_supervisor::supervisor_health();
    let config = app_state.app_config.config.load();
    let recording = config.recording.as_ref();
    Json(RecordingHealthResponse {
        enabled: recording.is_none_or(|cfg| cfg.enabled),
        server_time: chrono::Utc::now().timestamp(),
        reconciliation_last_run: health.reconciliation_last_run(),
        retention_last_tick: health.retention_last_tick(),
        retention_sweep_interval_secs: recording.and_then(|cfg| cfg.retention.as_ref().map(|r| r.sweep_interval_secs)),
        notification_last_drain: health.notification_last_drain(),
        notification_outbox_depth: health.notification_outbox_depth(),
        notification_dead_lettered: health.notification_dead_lettered(),
        queue_revision: app_state.downloads.revision.load(std::sync::atomic::Ordering::SeqCst),
    })
    .into_response()
}

#[derive(Debug, Clone, Serialize)]
pub struct RecordingHealthResponse {
    pub enabled: bool,
    pub server_time: i64,
    pub reconciliation_last_run: Option<i64>,
    pub retention_last_tick: Option<i64>,
    pub retention_sweep_interval_secs: Option<u64>,
    pub notification_last_drain: Option<i64>,
    pub notification_outbox_depth: i64,
    pub notification_dead_lettered: i64,
    pub queue_revision: u64,
}

fn rule_error_response(err: &crate::api::model::recording_rule_service::RuleServiceError) -> axum::response::Response {
    use crate::api::model::recording_rule_service::RuleServiceError;
    let status = match err {
        RuleServiceError::Forbidden
        | RuleServiceError::SharedManagementNotAdministrator
        | RuleServiceError::NotOwner => StatusCode::FORBIDDEN,
        RuleServiceError::InvalidRule | RuleServiceError::InvalidFuture | RuleServiceError::Unsupported { .. } => {
            StatusCode::BAD_REQUEST
        }
        RuleServiceError::UnknownRule => StatusCode::NOT_FOUND,
        RuleServiceError::PersistenceFailed | RuleServiceError::PartialOperation { .. } => {
            StatusCode::INTERNAL_SERVER_ERROR
        }
    };
    error_response(status, err.code())
}

fn recording_rule_repo(app_state: &AppState) -> RecordingRuleRepository {
    RecordingRuleRepository::new(&app_state.app_config.config.load().storage_dir)
}

fn can_write_rules(claims: &shared::model::Claims) -> bool { claims.permissions.contains(Permission::RecordingWrite) }

fn is_admin(claims: &shared::model::Claims) -> bool { claims.roles.iter().any(|role| role == ROLE_ADMIN) }

fn quota_limits_from_config(config: Option<&crate::model::RecordingQuotaConfig>) -> recording_quota::QuotaLimits {
    let mut per_user_bytes = std::collections::HashMap::new();
    if let Some(config) = config {
        for (user_id, bytes) in &config.per_user_bytes {
            per_user_bytes.insert(UserId::from(user_id.clone()), *bytes);
        }
        recording_quota::QuotaLimits {
            default_private_bytes: config.default_private_bytes,
            per_user_bytes,
            shared_bytes: config.shared_bytes,
        }
    } else {
        recording_quota::QuotaLimits::default()
    }
}

async fn all_recording_tasks(app_state: &AppState) -> Vec<FileDownload> {
    let mut tasks = Vec::new();
    let q = app_state.downloads.queue.lock().await;
    tasks.extend(q.iter().filter(|d| d.recording.is_some()).cloned());
    drop(q);
    let s = app_state.downloads.scheduled.read().await;
    tasks.extend(s.iter().filter(|d| d.recording.is_some()).cloned());
    drop(s);
    let a = app_state.downloads.active.read().await;
    tasks.extend(a.iter().filter(|d| d.recording.is_some()).cloned());
    drop(a);
    let f = app_state.downloads.finished.read().await;
    tasks.extend(f.iter().filter(|d| d.recording.is_some()).cloned());
    tasks
}

async fn resolve_recording_source(
    app_state: &Arc<AppState>,
    target_name: &str,
    virtual_id: &mut String,
    input_name: &mut String,
    cluster: XtreamCluster,
) -> Option<crate::api::endpoints::v1_api_playlist::ResolvedRecordingSource> {
    if recording_virtual_id(virtual_id).is_none() {
        if cluster != XtreamCluster::Live {
            return None;
        }
        let Some(resolved) =
            crate::api::endpoints::v1_api_playlist::resolve_target_live_recording_source_by_epg_channel(
                &app_state.app_config,
                target_name,
                virtual_id,
            )
            .await
        else {
            return None;
        };
        if !accept_resolved_recording_source(virtual_id, input_name, &resolved) {
            return None;
        }
    }
    let Some(virtual_id_value) = recording_virtual_id(virtual_id) else {
        return None;
    };
    let Some(resolved) = crate::api::endpoints::v1_api_playlist::resolve_target_recording_source(
        &app_state.app_config,
        target_name,
        input_name,
        virtual_id_value,
        cluster,
    )
    .await
    else {
        return None;
    };
    accept_resolved_recording_source(virtual_id, input_name, &resolved).then_some(resolved)
}

/// GET /api/v1/recording/rules
pub async fn list_recording_rules(
    State(app_state): State<Arc<AppState>>,
    AuthClaims(claims): AuthClaims,
) -> impl IntoResponse {
    if !claims.permissions.contains(Permission::RecordingRead) {
        return error_response(StatusCode::FORBIDDEN, "recording_forbidden");
    }
    let Some(subject_id) = claims.subject_id.as_ref() else {
        return error_response(StatusCode::UNAUTHORIZED, "recording_token_refresh_required");
    };
    let Ok(rules) = recording_rule_repo(&app_state).list().await else {
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, "recording_persistence_failed");
    };
    let admin = is_admin(&claims);
    let revision = app_state.downloads.revision.load(std::sync::atomic::Ordering::SeqCst);
    Json(
        rules
            .into_iter()
            .filter(|rule| rule.visibility == RuleVisibility::Shared || admin || &rule.owner_id == subject_id)
            .map(|rule| RecordingRuleResponse { revision, rule })
            .collect::<Vec<_>>(),
    )
    .into_response()
}

#[derive(Debug, Clone, Serialize)]
pub struct RecordingRuleResponse {
    /// Latest queue revision observed when the response was assembled.
    /// The frontend uses this to detect stale snapshots when polling.
    pub revision: u64,
    #[serde(flatten)]
    pub rule: RecordingRule,
}

/// POST /api/v1/recording/rules
pub async fn create_recording_rule(
    State(app_state): State<Arc<AppState>>,
    AuthClaims(claims): AuthClaims,
    Json(mut body): Json<CreateRecordingRuleBody>,
) -> impl IntoResponse {
    let Some(owner_id) = claims.subject_id.clone() else {
        return error_response(StatusCode::UNAUTHORIZED, "recording_token_refresh_required");
    };
    if resolve_recording_source(
        &app_state,
        &body.target_id,
        &mut body.virtual_id,
        &mut body.input_name,
        XtreamCluster::Live,
    )
    .await
    .is_none()
    {
        return rule_error_response(&RuleServiceError::InvalidRule);
    }
    let now = chrono::Utc::now().timestamp();
    let rule = recording_rule_from_create(owner_id, body, now);
    if let Err(err) = crate::api::model::recording_rule_service::validate_rule(&rule).and_then(|()| {
        crate::api::model::recording_rule_service::authorize_rule_action(
            can_write_rules(&claims),
            is_admin(&claims),
            &rule.owner_id,
            &rule,
        )
    }) {
        return rule_error_response(&err);
    }
    match recording_rule_repo(&app_state).create(rule).await {
        Ok(rule) => {
            let _ = app_state.event_manager.send_event(EventMessage::RecordingRulesChanged);
            Json(RecordingRuleResponse {
                revision: app_state.downloads.revision.load(std::sync::atomic::Ordering::SeqCst),
                rule,
            })
            .into_response()
        }
        Err(_) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "recording_persistence_failed"),
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateRecordingRuleBody {
    pub target_id: String,
    pub virtual_id: String,
    pub input_name: String,
    pub body: RuleBody,
    #[serde(default)]
    pub visibility: Option<RuleVisibility>,
    #[serde(default)]
    pub channel_id: Option<String>,
    #[serde(default)]
    pub pre_roll_secs: u64,
    #[serde(default)]
    pub post_roll_secs: u64,
}

fn recording_rule_from_create(owner_id: UserId, body: CreateRecordingRuleBody, now: i64) -> RecordingRule {
    RecordingRule {
        id: format!("rule-{}-{}", now, shared::utils::generate_random_string(8)),
        owner_id,
        visibility: body.visibility.unwrap_or_default(),
        enabled: true,
        source: RuleSource::new(body.target_id, body.virtual_id, body.input_name),
        channel_id: body.channel_id,
        body: body.body,
        pre_roll_secs: body.pre_roll_secs,
        post_roll_secs: body.post_roll_secs,
        created_at: now,
        updated_at: now,
    }
}

/// PATCH /api/v1/recording/rules/{id}
pub async fn edit_recording_rule(
    axum::extract::Path(id): axum::extract::Path<String>,
    State(app_state): State<Arc<AppState>>,
    AuthClaims(claims): AuthClaims,
    Json(body): Json<EditRecordingRuleBody>,
) -> impl IntoResponse {
    if claims.subject_id.is_none() {
        return error_response(StatusCode::UNAUTHORIZED, "recording_token_refresh_required");
    }
    let repo = recording_rule_repo(&app_state);
    let mut rule = match repo.load().await {
        Ok(file) => match file.rules.into_iter().find(|rule| rule.id == id) {
            Some(rule) => rule,
            None => {
                return rule_error_response(&crate::api::model::recording_rule_service::RuleServiceError::UnknownRule)
            }
        },
        Err(_) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, "recording_persistence_failed"),
    };
    if let Err(err) = authorize_and_apply_recording_rule_edit(&claims, &mut rule, body, chrono::Utc::now().timestamp())
    {
        return match err {
            EditRuleError::MissingSubject => {
                error_response(StatusCode::UNAUTHORIZED, "recording_token_refresh_required")
            }
            EditRuleError::Rule(err) => rule_error_response(&err),
        };
    }
    match repo.update(rule).await {
        Ok(Some(rule)) => {
            let _ = app_state.event_manager.send_event(EventMessage::RecordingRulesChanged);
            Json(RecordingRuleResponse {
                revision: app_state.downloads.revision.load(std::sync::atomic::Ordering::SeqCst),
                rule,
            })
            .into_response()
        }
        Ok(None) => rule_error_response(&crate::api::model::recording_rule_service::RuleServiceError::UnknownRule),
        Err(_) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "recording_persistence_failed"),
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct EditRecordingRuleBody {
    #[serde(default)]
    pub body: Option<RuleBody>,
    #[serde(default)]
    pub visibility: Option<RuleVisibility>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub channel_id: Option<String>,
    #[serde(default)]
    pub clear_channel_id: bool,
    #[serde(default)]
    pub pre_roll_secs: Option<u64>,
    #[serde(default)]
    pub post_roll_secs: Option<u64>,
}

#[derive(Debug, PartialEq, Eq)]
enum EditRuleError {
    MissingSubject,
    Rule(RuleServiceError),
}

fn authorize_and_apply_recording_rule_edit(
    claims: &shared::model::Claims,
    rule: &mut RecordingRule,
    body: EditRecordingRuleBody,
    now: i64,
) -> Result<(), EditRuleError> {
    let subject_id = claims.subject_id.as_ref().ok_or(EditRuleError::MissingSubject)?;
    crate::api::model::recording_rule_service::authorize_rule_action(
        can_write_rules(claims),
        is_admin(claims),
        subject_id,
        rule,
    )
    .map_err(EditRuleError::Rule)?;
    apply_recording_rule_edit(rule, body, now);
    crate::api::model::recording_rule_service::validate_rule(rule).map_err(EditRuleError::Rule)?;
    crate::api::model::recording_rule_service::authorize_rule_action(
        can_write_rules(claims),
        is_admin(claims),
        subject_id,
        rule,
    )
    .map_err(EditRuleError::Rule)
}

fn apply_recording_rule_edit(rule: &mut RecordingRule, body: EditRecordingRuleBody, now: i64) {
    if body.clear_channel_id {
        rule.channel_id = None;
    } else if let Some(channel_id) = body.channel_id {
        rule.channel_id = Some(channel_id);
    }
    if let Some(pre_roll_secs) = body.pre_roll_secs {
        rule.pre_roll_secs = pre_roll_secs;
    }
    if let Some(post_roll_secs) = body.post_roll_secs {
        rule.post_roll_secs = post_roll_secs;
    }
    if let Some(visibility) = body.visibility {
        rule.visibility = visibility;
    }
    if let Some(enabled) = body.enabled {
        rule.enabled = enabled;
    }
    if let Some(rule_body) = body.body {
        rule.body = rule_body;
    }
    rule.updated_at = now;
}

fn recording_virtual_id(virtual_id: &str) -> Option<u32> { virtual_id.parse::<u32>().ok() }

fn accept_resolved_recording_source(
    virtual_id: &mut String,
    input_name: &mut String,
    resolved: &crate::api::endpoints::v1_api_playlist::ResolvedRecordingSource,
) -> bool {
    if !input_name.trim().is_empty() && input_name != &resolved.input_name {
        return false;
    }
    *virtual_id = resolved.virtual_id.to_string();
    input_name.clone_from(&resolved.input_name);
    true
}

/// DELETE /api/v1/recording/rules/{id}
#[derive(Debug, Clone, Deserialize)]
pub struct DeleteRuleParams {
    #[serde(default)]
    pub future: Option<String>,
}

pub async fn delete_recording_rule(
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(params): axum::extract::Query<DeleteRuleParams>,
    State(app_state): State<Arc<AppState>>,
    AuthClaims(claims): AuthClaims,
) -> impl IntoResponse {
    let future = match crate::api::model::recording_rule_service::validate_delete(params.future.as_deref()) {
        Ok(future) => future,
        Err(err) => return rule_error_response(&err),
    };
    let repo = recording_rule_repo(&app_state);
    let rule = match repo.load().await {
        Ok(file) => match file.rules.into_iter().find(|rule| rule.id == id) {
            Some(rule) => rule,
            None => {
                return rule_error_response(&crate::api::model::recording_rule_service::RuleServiceError::UnknownRule)
            }
        },
        Err(_) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, "recording_persistence_failed"),
    };
    let Some(subject_id) = claims.subject_id.as_ref() else {
        return error_response(StatusCode::UNAUTHORIZED, "recording_token_refresh_required");
    };
    if let Err(err) = crate::api::model::recording_rule_service::authorize_rule_action(
        can_write_rules(&claims),
        is_admin(&claims),
        subject_id,
        &rule,
    ) {
        return rule_error_response(&err);
    }
    // Deleting a rule with `future=cancel` writes to two stores: the
    // queue (cancel the upcoming occurrences) and the rule repository
    // (drop the rule). They cannot commit together, so the queue side
    // hands back everything it cancelled and this handler replays it if
    // the rule store then fails. Without the compensation the operator
    // was left with the rule still present and its upcoming recordings
    // silently gone.
    let service = RecordingService::new(app_state.downloads.clone(), app_state.app_config.clone());
    let mut cancelled = Vec::new();
    if future == DeleteFuture::Cancel {
        match service.cancel_future_rule_recordings(&claims, &id, chrono::Utc::now().timestamp()).await {
            Ok(tasks) => cancelled = tasks,
            Err(_) => {
                return rule_error_response(&RuleServiceError::PartialOperation {
                    primary: "rule_retained".to_string(),
                    secondary: "future_cancel_failed".to_string(),
                });
            }
        }
    }
    match repo.delete(&id).await {
        Ok(true) => {
            // Cancelling future rule recordings mutates the queue, so the
            // frontend needs both the rules change and a snapshot refresh.
            let _ = app_state.event_manager.send_event(EventMessage::RecordingChanged);
            let _ = app_state.event_manager.send_event(EventMessage::RecordingRulesChanged);
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => {
            // Nothing was deleted, so nothing should have been cancelled.
            restore_or_report_partial(&service, &cancelled, &app_state, || {
                rule_error_response(&RuleServiceError::UnknownRule)
            })
            .await
        }
        Err(_) => {
            restore_or_report_partial(&service, &cancelled, &app_state, || {
                error_response(StatusCode::INTERNAL_SERVER_ERROR, "recording_persistence_failed")
            })
            .await
        }
    }
}

/// Undo a rule-delete's queue-side cancel and report `on_restored`. If
/// the undo itself fails there is nothing left to try: report the
/// partial operation so the operator knows the two stores disagree and
/// which side won.
async fn restore_or_report_partial<F>(
    service: &RecordingService,
    cancelled: &[crate::api::model::recording_service::CancelledRuleRecording],
    app_state: &Arc<AppState>,
    on_restored: F,
) -> axum::response::Response
where
    F: FnOnce() -> axum::response::Response,
{
    if cancelled.is_empty() {
        return on_restored();
    }
    match service.restore_cancelled_rule_recordings(cancelled).await {
        Ok(()) => {
            let _ = app_state.event_manager.send_event(EventMessage::RecordingChanged);
            on_restored()
        }
        Err(err) => {
            log::error!(
                "failed to restore {} cancelled rule recordings after a failed rule delete: {err}",
                cancelled.len()
            );
            rule_error_response(&RuleServiceError::PartialOperation {
                primary: "future_cancelled".to_string(),
                secondary: "rule_delete_failed".to_string(),
            })
        }
    }
}

/// GET /api/v1/recording/availability — authenticated DVR preflight.
pub async fn recording_availability(
    State(app_state): State<Arc<AppState>>,
    AuthClaims(claims): AuthClaims,
) -> axum::response::Response {
    if !claims.permissions.contains(Permission::RecordingRead)
        && !claims.permissions.contains(Permission::RecordingWrite)
    {
        return error_response(StatusCode::FORBIDDEN, "recording_forbidden");
    }
    if !crate::api::model::recording::recording_supervisor::recording_enabled(&app_state.app_config) {
        return error_response(StatusCode::NOT_IMPLEMENTED, "recording_disabled");
    }
    StatusCode::NO_CONTENT.into_response()
}

pub fn recording_availability_register(router: Router<Arc<AppState>>) -> Router<Arc<AppState>> {
    router.route("/recording/availability", get(recording_availability))
}

/// Reject every recording route while the DVR is switched off.
///
/// `config.recording.enabled: false` has to mean more than "supervisors
/// idle": a client that keeps calling the routes would otherwise keep
/// creating recordings nothing will ever run. One layer on the nested
/// router covers every route, so a route added later is gated
/// automatically.
///
/// `501 Not Implemented` with the stable code `recording_disabled`
/// distinguishes "switched off here" from `403` (not allowed) and `404`
/// (does not exist).
pub async fn require_recording_enabled(
    State(app_state): State<Arc<AppState>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if crate::api::model::recording::recording_supervisor::recording_enabled(&app_state.app_config) {
        next.run(request).await
    } else {
        error_response(StatusCode::NOT_IMPLEMENTED, "recording_disabled")
    }
}

/// Build the recording router for `/api/v1/recording`.
///
/// The router is a thin wrapper. The state type is `Arc<AppState>`
/// (which matches the rest of the v1 router tree). Production wiring
/// in `v1_api::router_v1` calls `recording_api_register(router)` which
/// merges the recording routes into the v1 router tree.
pub fn recording_api_register(router: Router<Arc<AppState>>) -> axum::Router<Arc<AppState>> {
    let recording_routes = Router::new()
        .route("/tasks", get(list_recording_tasks).post(create_recording_task))
        .route("/tasks/{id}", patch(edit_recording_task).delete(delete_recording_task))
        .route("/tasks/{id}/cancel", post(cancel_recording_task))
        .route("/tasks/{id}/pause", post(pause_recording_task))
        .route("/tasks/{id}/resume", post(resume_recording_task))
        .route("/tasks/{id}/retry", post(retry_recording_task))
        .route("/tasks/{id}/remove", axum::routing::delete(remove_recording_task))
        .route("/conflicts/preview", post(preview_recording_conflicts))
        .route("/quota", get(get_recording_quota))
        .route("/health", get(get_recording_health))
        .route("/rules", get(list_recording_rules).post(create_recording_rule))
        .route("/rules/{id}", patch(edit_recording_rule).delete(delete_recording_rule));

    router.nest("/recording", recording_routes)
}

/// The `recording.enabled` gate as a router layer.
///
/// A macro rather than a function so the caller never has to name the
/// opaque `FromFnLayer<..>` type — the same reason `permission_layer!`
/// is a macro.
#[macro_export]
macro_rules! recording_enabled_layer {
    ($app_state:expr) => {{
        let app_state = ::std::sync::Arc::clone($app_state);
        ::axum::middleware::from_fn_with_state(app_state, move |state, request, next| {
            $crate::api::endpoints::recording_api::require_recording_enabled(state, request, next)
        })
    }};
}
pub use recording_enabled_layer;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn edit_claims(subject_id: Option<UserId>, admin: bool) -> shared::model::Claims {
        shared::model::Claims {
            username: "alice".to_string(),
            iss: "tuliprox".to_string(),
            iat: 0,
            exp: 0,
            roles: admin.then(|| ROLE_ADMIN.to_string()).into_iter().collect(),
            permissions: Permission::RecordingWrite.into(),
            pwd_version: 0,
            subject_id,
            permission_schema_version: shared::model::CURRENT_PERMISSION_SCHEMA_VERSION,
        }
    }

    fn enabled_recording_state() -> Arc<AppState> {
        let recording_dto = shared::model::RecordingConfigDto { enabled: true, ..Default::default() };
        let recording_runtime = crate::model::RecordingConfig::from(&recording_dto);
        crate::api::model::create_test_app_state(crate::model::Config {
            recording: Some(recording_runtime),
            ..crate::model::Config::default()
        })
    }

    #[tokio::test]
    async fn recording_availability_accepts_write_only_claims() {
        let response = recording_availability(
            State(enabled_recording_state()),
            AuthClaims(edit_claims(Some(UserId::from("web:alice")), false)),
        )
        .await;

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn recording_availability_accepts_read_only_claims() {
        let mut claims = edit_claims(Some(UserId::from("web:alice")), false);
        claims.permissions = Permission::RecordingRead.into();

        let response = recording_availability(State(enabled_recording_state()), AuthClaims(claims)).await;

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn recording_availability_rejects_claims_without_recording_permission() {
        let app_state = crate::api::model::create_test_app_state(crate::model::Config::default());
        let mut claims = edit_claims(Some(UserId::from("web:alice")), false);
        claims.permissions = Permission::SystemRead.into();

        let response = recording_availability(State(app_state), AuthClaims(claims)).await;

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await;
        assert!(matches!(body.as_deref(), Ok(bytes) if bytes == br#"{"error":"recording_forbidden"}"#));
    }

    fn editable_rule() -> RecordingRule {
        RecordingRule {
            id: "rule-1".to_string(),
            owner_id: UserId::from("web:alice"),
            visibility: RuleVisibility::Private,
            enabled: true,
            source: RuleSource::new("7", "42", "input-a"),
            channel_id: Some("channel-1".to_string()),
            body: RuleBody::WeeklyTimeslot {
                weekday: 3,
                local_start_time: "20:00".to_string(),
                duration_secs: 3600,
                timezone: "UTC".to_string(),
            },
            pre_roll_secs: 0,
            post_roll_secs: 0,
            created_at: 1,
            updated_at: 1,
        }
    }

    #[test]
    fn edit_rule_requires_subject_id() {
        let mut rule = editable_rule();
        let result = authorize_and_apply_recording_rule_edit(
            &edit_claims(None, false),
            &mut rule,
            EditRecordingRuleBody::default(),
            2,
        );

        assert!(matches!(result, Err(EditRuleError::MissingSubject)));
    }

    #[test]
    fn owner_cannot_promote_private_rule_to_shared() {
        let mut rule = editable_rule();
        let patch: EditRecordingRuleBody = serde_json::from_value(json!({"visibility": "shared"})).expect("parse");
        let result = authorize_and_apply_recording_rule_edit(
            &edit_claims(Some(UserId::from("web:alice")), false),
            &mut rule,
            patch,
            2,
        );

        assert!(matches!(result, Err(EditRuleError::Rule(RuleServiceError::SharedManagementNotAdministrator))));
    }

    #[test]
    fn admin_can_promote_private_rule_to_shared() {
        let mut rule = editable_rule();
        let patch: EditRecordingRuleBody = serde_json::from_value(json!({"visibility": "shared"})).expect("parse");

        authorize_and_apply_recording_rule_edit(&edit_claims(Some(UserId::builtin_admin()), true), &mut rule, patch, 2)
            .expect("admin edit");

        assert_eq!(rule.visibility, RuleVisibility::Shared);
    }

    #[test]
    fn edit_rule_clear_channel_id_is_distinct_from_unchanged() {
        let mut unchanged = editable_rule();
        apply_recording_rule_edit(&mut unchanged, EditRecordingRuleBody::default(), 2);
        assert_eq!(unchanged.channel_id.as_deref(), Some("channel-1"));

        let mut cleared = editable_rule();
        let patch: EditRecordingRuleBody = serde_json::from_value(json!({"clear_channel_id": true})).expect("parse");
        apply_recording_rule_edit(&mut cleared, patch, 2);
        assert!(cleared.channel_id.is_none());
    }

    #[test]
    fn recording_virtual_id_rejects_non_numeric_and_overflow() {
        assert_eq!(recording_virtual_id("42"), Some(42));
        assert_eq!(recording_virtual_id("epg-channel"), None);
        assert_eq!(recording_virtual_id("4294967296"), None);
    }

    #[test]
    fn resolved_source_rejects_conflicting_input_and_canonicalizes_id() {
        let resolved = crate::api::endpoints::v1_api_playlist::ResolvedRecordingSource {
            virtual_id: 42,
            input_name: "input-a".to_string(),
            title: "Example".to_string(),
            extension: Some("ts".to_string()),
            downloadable: true,
        };
        let mut virtual_id = "00042".to_string();
        let mut input_name = "input-b".to_string();
        assert!(!accept_resolved_recording_source(&mut virtual_id, &mut input_name, &resolved));
        assert_eq!(input_name, "input-b");

        input_name.clear();
        assert!(accept_resolved_recording_source(&mut virtual_id, &mut input_name, &resolved));
        assert_eq!(virtual_id, "42");
        assert_eq!(input_name, "input-a");
    }

    #[test]
    fn create_rule_body_accepts_string_target_and_weekly_body() {
        let parsed: CreateRecordingRuleBody = serde_json::from_value(json!({
            "target_id": "default",
            "virtual_id": "42",
            "input_name": "input-a",
            "body": {
                "kind": "weekly_timeslot",
                "weekday": 3,
                "local_start_time": "20:00",
                "duration_secs": 3600,
                "timezone": "Europe/Berlin"
            }
        }))
        .expect("parse weekly rule");

        assert_eq!(parsed.target_id, "default");
        assert!(matches!(parsed.body, RuleBody::WeeklyTimeslot { weekday: 3, .. }));
    }

    #[test]
    fn create_rule_body_accepts_new_episode_body() {
        let parsed: CreateRecordingRuleBody = serde_json::from_value(json!({
            "target_id": "default",
            "virtual_id": "42",
            "input_name": "input-a",
            "body": {
                "kind": "new_episode",
                "series_id": "series-1",
                "title_pattern": null,
                "exclude_repeat": true
            }
        }))
        .expect("parse new episode rule");

        assert!(matches!(
            parsed.body,
            RuleBody::NewEpisode { series_id: Some(ref id), exclude_repeat: true, .. } if id == "series-1"
        ));
    }

    #[test]
    fn create_rule_body_rejects_numeric_target() {
        let parsed = serde_json::from_value::<CreateRecordingRuleBody>(json!({
            "target_id": 7,
            "virtual_id": "42",
            "input_name": "input-a",
            "body": {
                "kind": "weekly_timeslot",
                "weekday": 3,
                "local_start_time": "20:00",
                "duration_secs": 3600,
                "timezone": "UTC"
            }
        }));

        assert!(parsed.is_err());
    }

    #[test]
    fn create_rule_preserves_new_episode_body() {
        let body: CreateRecordingRuleBody = serde_json::from_value(json!({
            "target_id": "default",
            "virtual_id": "42",
            "input_name": "input-a",
            "body": {
                "kind": "new_episode",
                "series_id": "series-1",
                "title_pattern": null,
                "exclude_repeat": true
            }
        }))
        .expect("parse new episode rule");

        let rule = recording_rule_from_create(UserId::from("web:alice"), body, 123);

        assert_eq!(rule.source.target_id, "default");
        assert!(matches!(rule.body, RuleBody::NewEpisode { series_id: Some(ref id), .. } if id == "series-1"));
    }

    #[test]
    fn create_recording_source_rejects_numeric_target() {
        let parsed = serde_json::from_value::<CreateRecordingSourceBody>(json!({
            "target_id": 7,
            "virtual_id": "42",
            "cluster": "Live",
            "input_name": "input-a"
        }));

        assert!(parsed.is_err());
    }

    #[test]
    fn create_recording_source_accepts_string_target() {
        let parsed: CreateRecordingSourceBody = serde_json::from_value(json!({
            "target_id": "default",
            "virtual_id": "42",
            "cluster": "Live",
            "input_name": "input-a"
        }))
        .expect("parse recording source");

        assert_eq!(parsed.target_id, "default");
    }

    #[test]
    fn edit_rule_body_round_trips_enabled_field() {
        let wire = r#"{"enabled":false,"visibility":"shared"}"#;
        let parsed: EditRecordingRuleBody = serde_json::from_str(wire).expect("parse");
        assert_eq!(parsed.enabled, Some(false));
        assert!(parsed.visibility.is_some());
        assert!(parsed.body.is_none());
    }

    #[test]
    fn edit_rule_body_replaces_variant() {
        let parsed: EditRecordingRuleBody = serde_json::from_value(json!({
            "body": {
                "kind": "new_episode",
                "series_id": null,
                "title_pattern": "News",
                "exclude_repeat": false
            }
        }))
        .expect("parse edit body");

        assert!(matches!(
            parsed.body,
            Some(RuleBody::NewEpisode { title_pattern: Some(ref title), exclude_repeat: false, .. }) if title == "News"
        ));
    }

    #[test]
    fn apply_edit_rule_body_switches_variant() {
        let mut rule = RecordingRule {
            id: "rule-1".to_string(),
            owner_id: UserId::from("web:alice"),
            visibility: RuleVisibility::Private,
            enabled: true,
            source: RuleSource::new("7", "42", "input-a"),
            channel_id: None,
            body: RuleBody::WeeklyTimeslot {
                weekday: 3,
                local_start_time: "20:00".to_string(),
                duration_secs: 3600,
                timezone: "UTC".to_string(),
            },
            pre_roll_secs: 0,
            post_roll_secs: 0,
            created_at: 1,
            updated_at: 1,
        };
        let patch: EditRecordingRuleBody = serde_json::from_value(json!({
            "body": {
                "kind": "new_episode",
                "series_id": null,
                "title_pattern": "News",
                "exclude_repeat": false
            }
        }))
        .expect("parse edit body");

        apply_recording_rule_edit(&mut rule, patch, 2);

        assert!(matches!(
            rule.body,
            RuleBody::NewEpisode { title_pattern: Some(ref title), exclude_repeat: false, .. } if title == "News"
        ));
        assert_eq!(rule.updated_at, 2);
    }

    #[test]
    fn error_response_round_trip_serializes_code() {
        let body = serde_json::to_value(&ErrorResponse { error: "recording_unknown" }).expect("serialize");
        assert_eq!(body, json!({"error": "recording_unknown"}));
    }

    #[test]
    fn list_tasks_params_accepts_missing_owner_and_visibility() {
        let parsed: ListTasksParams = serde_json::from_value(json!({})).expect("parse empty params");
        assert!(parsed.owner.is_none());
        assert!(parsed.visibility.is_none());
    }

    #[test]
    fn list_tasks_params_accepts_explicit_owner_and_visibility() {
        let parsed: ListTasksParams =
            serde_json::from_value(json!({"owner": "web:alice", "visibility": "private"})).expect("parse full params");
        assert_eq!(parsed.owner.as_deref(), Some("web:alice"));
        assert_eq!(parsed.visibility.as_deref(), Some("private"));
    }

    #[test]
    fn delete_rule_params_accepts_missing_future_query() {
        let parsed: DeleteRuleParams = serde_json::from_value(json!({})).expect("parse empty");
        assert!(parsed.future.is_none());
    }

    #[test]
    fn delete_rule_params_accepts_future_retain() {
        let parsed: DeleteRuleParams = serde_json::from_value(json!({"future": "retain"})).expect("parse retain");
        assert_eq!(parsed.future.as_deref(), Some("retain"));
    }

    #[test]
    fn delete_rule_params_accepts_future_cancel() {
        let parsed: DeleteRuleParams = serde_json::from_value(json!({"future": "cancel"})).expect("parse cancel");
        assert_eq!(parsed.future.as_deref(), Some("cancel"));
    }
}
