//! Recording REST routes.

use crate::{
    api::{
        endpoints::recording_media_api::AuthClaims,
        model::{
            recording_rule_service::{DeleteFuture, RuleServiceError},
            recording_service::{
                CreateMediaRecordingInput, CreateRecordingInput, EditRecordingPatch, IdempotencyRequest,
                RecordingService, RecordingSourceInput, ServiceError,
            },
            AppState,
        },
    },
    model::{ConfigTarget, ProxyUserCredentials},
    repository::recording_rule_repository::RecordingRuleRepository,
};
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, patch, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use shared::model::{
    recording::{CreateRecordingRequest, RecordingProvenance, RecordingSourceRequest},
    recording_rule::{RecordingRule, RuleBody, RuleSource, RuleVisibility},
    EventMessage, Permission, UserId, XtreamCluster,
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

/// Start the transfer worker after a command has made work runnable.
///
/// Pause deliberately stops the worker while leaving the task in the active
/// slot. Retry moves a terminal task back to the queue. Neither state mutation
/// owns the runtime dependencies needed to spawn a worker, so command handlers
/// must do that here after the mutation has committed.
async fn start_recording_worker_if_needed(app_state: &AppState) -> Result<(), &'static str> {
    let recording_config = app_state.app_config.config.load().recording().cloned().ok_or("recording_disabled")?;
    tuliprox_dvr::recording::recording_transfer::resume_recording_worker_if_needed(
        &app_state.recording_ctx(),
        &recording_config,
    )
    .await
    .map_err(|_| "recording_worker_failed")
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
        | ServiceError::QuotaExceeded
        | ServiceError::ConcurrencyExceeded => StatusCode::BAD_REQUEST,
        ServiceError::DiskFull => StatusCode::INSUFFICIENT_STORAGE,
        ServiceError::Disabled => StatusCode::NOT_IMPLEMENTED,
        ServiceError::Forbidden | ServiceError::SharedCreationNotAdministrator => StatusCode::FORBIDDEN,
        ServiceError::UnknownRecording => StatusCode::NOT_FOUND,
        ServiceError::IdempotencyConflict => StatusCode::CONFLICT,
        // A replay is the original success, answered again.
        ServiceError::IdempotentReplay { .. } => StatusCode::NO_CONTENT,
        ServiceError::PersistenceFailed | ServiceError::IoError(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// The `Idempotency-Key` header, with a digest of the body it arrived with.
///
/// The digest is what makes a replay distinguishable from a different
/// request wearing the same key. It is taken over the canonical JSON of the
/// request, so field order in the original submission cannot change it.
fn idempotency_from_headers(headers: &HeaderMap, body: &CreateRecordingRequest) -> Option<IdempotencyRequest> {
    let key = headers.get("Idempotency-Key")?.to_str().ok()?.trim();
    if key.is_empty() {
        return None;
    }
    let canonical = serde_json::to_string(body).unwrap_or_default();
    Some(IdempotencyRequest {
        key: key.to_string(),
        fingerprint: blake3::hash(canonical.as_bytes()).to_hex().to_string(),
    })
}

/// POST /api/v1/recording/requests
///
/// A command, not a query: it answers `204` and nothing else. The new
/// entry reaches the caller on the recording snapshot like every other
/// change, so there is one description of a recording rather than two
/// that can disagree.
pub async fn create_recording_request(
    State(app_state): State<Arc<AppState>>,
    AuthClaims(claims): AuthClaims,
    headers: HeaderMap,
    Json(body): Json<CreateRecordingRequest>,
) -> impl IntoResponse {
    let idempotency = idempotency_from_headers(&headers, &body);
    let mut source = body.source.clone();
    let Some(resolved_source) = resolve_recording_source(
        &app_state,
        &source.target_id,
        &mut source.virtual_id,
        &mut source.input_name,
        source.cluster,
        body.channel_id.as_deref(),
        body.channel_name.as_deref(),
    )
    .await
    else {
        return service_error_response(&ServiceError::InvalidSource);
    };
    if let Err(err) = authorize_api_user_source(
        &app_state,
        &claims,
        &source.target_id,
        resolved_source.virtual_id,
        Some(source.cluster),
    )
    .await
    {
        return service_error_response(&err);
    }
    if source.cluster != XtreamCluster::Live {
        return create_http_recording_task(&app_state, &claims, body, source, resolved_source, idempotency).await;
    }
    let (Some(program_start), Some(program_end)) = (body.program_start, body.program_end) else {
        return error_response(StatusCode::UNPROCESSABLE_ENTITY, "recording_invalid_interval");
    };
    let service = RecordingService::new(app_state.recording.queue.clone(), app_state.app_config.clone());
    let input = CreateRecordingInput {
        source: RecordingSourceInput {
            target_id: source.target_id,
            virtual_id: source.virtual_id,
            cluster: source.cluster,
            input_name: source.input_name,
            provider_id: resolved_source.provider_id,
            is_group: resolved_source.is_group,
        },
        program_title: body.program_title,
        program_start,
        program_end,
        pre_roll_secs: body.pre_roll_secs.unwrap_or_default(),
        post_roll_secs: body.post_roll_secs.unwrap_or_default(),
        visibility: body.visibility,
        channel_id: body.channel_id,
        channel_name: body.channel_name,
        group: resolved_source.group,
        provenance: RecordingProvenance::default(),
        epg: body.epg,
    };
    match service.create_recording_idempotent(&claims, &input, idempotency).await {
        Ok(_) => {
            let _ = app_state.event_manager.send_event(EventMessage::RecordingChanged);
            StatusCode::NO_CONTENT.into_response()
        }
        // The recording already exists from the first attempt; nothing new
        // to announce, and the caller gets the same answer it got then.
        Err(ServiceError::IdempotentReplay { .. }) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => service_error_response(&err),
    }
}

async fn create_http_recording_task(
    app_state: &Arc<AppState>,
    claims: &shared::model::Claims,
    body: CreateRecordingRequest,
    source: RecordingSourceRequest,
    resolved: crate::api::endpoints::v1_api_playlist::ResolvedRecordingSource,
    idempotency: Option<IdempotencyRequest>,
) -> axum::response::Response {
    if body.program_start.is_some()
        || body.program_end.is_some()
        || body.pre_roll_secs.is_some()
        || body.post_roll_secs.is_some()
    {
        return error_response(StatusCode::UNPROCESSABLE_ENTITY, "recording_invalid_interval");
    }
    if !resolved.downloadable {
        return error_response(StatusCode::UNPROCESSABLE_ENTITY, "recording_invalid_source");
    }
    let Some(extension) = resolved.extension.filter(|extension| !extension.is_empty()) else {
        return error_response(StatusCode::UNPROCESSABLE_ENTITY, "recording_invalid_path");
    };
    let input = CreateMediaRecordingInput {
        source: RecordingSourceInput {
            target_id: source.target_id,
            virtual_id: source.virtual_id,
            cluster: source.cluster,
            input_name: source.input_name,
            provider_id: resolved.provider_id,
            is_group: resolved.is_group,
        },
        title: resolved.title,
        extension,
        visibility: body.visibility,
        group: resolved.group,
        series_name: resolved.series_name,
    };
    let service = RecordingService::new(app_state.recording.queue.clone(), app_state.app_config.clone());
    match service.create_media_recording_idempotent(claims, &input, idempotency).await {
        Ok(_) => {}
        // The transfer already exists from the first attempt; nothing new to
        // announce, and the caller gets the same answer it got then.
        Err(ServiceError::IdempotentReplay { .. }) => return StatusCode::NO_CONTENT.into_response(),
        Err(err) => return service_error_response(&err),
    }
    if let Err(error) = start_recording_worker_if_needed(app_state).await {
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, error);
    }
    let _ = app_state.event_manager.send_event(EventMessage::RecordingChanged);
    StatusCode::NO_CONTENT.into_response()
}

/// PATCH /api/v1/recording/requests/{id}
pub async fn edit_recording_task(
    axum::extract::Path(id): axum::extract::Path<String>,
    State(app_state): State<Arc<AppState>>,
    AuthClaims(claims): AuthClaims,
    Json(body): Json<EditRecordingTaskBody>,
) -> impl IntoResponse {
    let service = RecordingService::new(app_state.recording.queue.clone(), app_state.app_config.clone());
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

/// POST /api/v1/recording/materializations/{id}/pause
///
/// Administrator-only: a materialization can be shared, so pausing it
/// stops a transfer other users are attached to and are still being
/// charged for.
pub async fn pause_recording_task(
    axum::extract::Path(id): axum::extract::Path<String>,
    State(app_state): State<Arc<AppState>>,
    AuthClaims(claims): AuthClaims,
) -> impl IntoResponse {
    if !is_admin(&claims) {
        return error_response(StatusCode::FORBIDDEN, "recording_forbidden");
    }
    let service = RecordingService::new(app_state.recording.queue.clone(), app_state.app_config.clone());
    match service.pause_recording(&claims, &id).await {
        Ok(()) => {
            let _ = app_state.event_manager.send_event(EventMessage::RecordingChanged);
            StatusCode::NO_CONTENT.into_response()
        }
        Err(err) => service_error_response(&err),
    }
}

/// POST /api/v1/recording/materializations/{id}/resume
///
/// Administrator-only, for the same reason as pause.
pub async fn resume_recording_task(
    axum::extract::Path(id): axum::extract::Path<String>,
    State(app_state): State<Arc<AppState>>,
    AuthClaims(claims): AuthClaims,
) -> impl IntoResponse {
    if !is_admin(&claims) {
        return error_response(StatusCode::FORBIDDEN, "recording_forbidden");
    }
    let service = RecordingService::new(app_state.recording.queue.clone(), app_state.app_config.clone());
    match service.resume_recording(&claims, &id).await {
        Ok(true) => {
            if let Err(error) = start_recording_worker_if_needed(&app_state).await {
                return error_response(StatusCode::INTERNAL_SERVER_ERROR, error);
            }
            let _ = app_state.event_manager.send_event(EventMessage::RecordingChanged);
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => service_error_response(&ServiceError::InvalidState),
        Err(err) => service_error_response(&err),
    }
}

/// POST /api/v1/recording/materializations/{id}/retry
///
/// Administrator-only, for the same reason as pause.
pub async fn retry_recording_task(
    axum::extract::Path(id): axum::extract::Path<String>,
    State(app_state): State<Arc<AppState>>,
    AuthClaims(claims): AuthClaims,
) -> impl IntoResponse {
    if !is_admin(&claims) {
        return error_response(StatusCode::FORBIDDEN, "recording_forbidden");
    }
    let service = RecordingService::new(app_state.recording.queue.clone(), app_state.app_config.clone());
    match service.retry_recording(&claims, &id).await {
        Ok(true) => {
            if let Err(error) = start_recording_worker_if_needed(&app_state).await {
                return error_response(StatusCode::INTERNAL_SERVER_ERROR, error);
            }
            let _ = app_state.event_manager.send_event(EventMessage::RecordingChanged);
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => service_error_response(&ServiceError::InvalidState),
        Err(err) => service_error_response(&err),
    }
}

/// POST /api/v1/recording/materializations/{id}/restart
///
/// The UI calls this only after the viewer confirms discarding a partial
/// whose provider ignored a byte-range request.
pub async fn restart_recording_task(
    axum::extract::Path(id): axum::extract::Path<String>,
    State(app_state): State<Arc<AppState>>,
    AuthClaims(claims): AuthClaims,
) -> impl IntoResponse {
    if !is_admin(&claims) {
        return error_response(StatusCode::FORBIDDEN, "recording_forbidden");
    }
    let service = RecordingService::new(app_state.recording.queue.clone(), app_state.app_config.clone());
    match service.restart_recording(&claims, &id).await {
        Ok(true) => {
            if let Err(error) = start_recording_worker_if_needed(&app_state).await {
                return error_response(StatusCode::INTERNAL_SERVER_ERROR, error);
            }
            let _ = app_state.event_manager.send_event(EventMessage::RecordingChanged);
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => service_error_response(&ServiceError::InvalidState),
        Err(err) => service_error_response(&err),
    }
}

/// DELETE /api/v1/recording/tasks/{id}
pub async fn remove_recording_task(
    axum::extract::Path(id): axum::extract::Path<String>,
    State(app_state): State<Arc<AppState>>,
    AuthClaims(claims): AuthClaims,
) -> impl IntoResponse {
    let service = RecordingService::new(app_state.recording.queue.clone(), app_state.app_config.clone());
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
    let service = RecordingService::new(app_state.recording.queue.clone(), app_state.app_config.clone());
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
    let service = RecordingService::new(app_state.recording.queue.clone(), app_state.app_config.clone());
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
    let mut source = RecordingSourceInput::from(&body.source);
    // EPG rows do not know the input, so the preview resolves the same
    // playlist channel the create request will record.
    let Some(resolved_source) = resolve_recording_source(
        &state,
        &body.source.target_name,
        &mut source.virtual_id,
        &mut source.input_name,
        source.cluster,
        body.source.channel_id.as_deref(),
        body.source.channel_name.as_deref(),
    )
    .await
    else {
        return service_error_response(&ServiceError::InvalidSource);
    };
    if let Err(err) =
        authorize_api_user_source(&state, &claims, &source.target_id, resolved_source.virtual_id, Some(source.cluster))
            .await
    {
        return service_error_response(&err);
    }
    let mut source_input = source;
    source_input.provider_id = resolved_source.provider_id;
    source_input.is_group = resolved_source.is_group;
    let request = crate::api::model::recording_service::ConflictPreviewRequest {
        source: source_input,
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
    /// EPG channel id for rows without a playlist id.
    #[serde(default)]
    pub channel_id: Option<String>,
    /// Display title that picks among streams sharing one EPG id.
    #[serde(default)]
    pub channel_name: Option<String>,
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
            provider_id: None,
            is_group: false,
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
    if !claims.is_admin() {
        return error_response(StatusCode::FORBIDDEN, "recording_forbidden");
    }
    let health = crate::api::model::recording::recording_supervisor::supervisor_health();
    let config = app_state.app_config.config.load();
    let recording = config.recording();
    Json(RecordingHealthResponse {
        enabled: recording.is_none_or(|cfg| cfg.enabled),
        server_time: chrono::Utc::now().timestamp(),
        reconciliation_last_run: health.reconciliation_last_run(),
        retention_last_tick: health.retention_last_tick(),
        retention_sweep_interval_secs: recording.and_then(|cfg| cfg.retention.as_ref().map(|r| r.sweep_interval_secs)),
        notification_last_drain: health.notification_last_drain(),
        // Read from the promoted outbox: the supervisor-local counter stopped
        // being updated when the outbox moved into `tuliprox-messaging`.
        notification_outbox_depth: tuliprox_messaging::outbox::health()
            .outbox_depth
            .load(std::sync::atomic::Ordering::Relaxed),
        notification_dead_lettered: health.notification_dead_lettered(),
        queue_revision: app_state.recording.queue.revision.load(std::sync::atomic::Ordering::SeqCst),
        recovery: app_state.recording.queue.recovery_health().await.map(Into::into),
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
    /// Absent when the queue is not repository backed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery: Option<RecordingRecoveryHealthResponse>,
}

/// The recovery contract, rendered for an operator tool.
///
/// Mapped rather than serialized directly: `tuliprox-btree` carries no
/// application concerns, so its health type deliberately has no `Serialize`.
/// Every field here is already redacted at the source — no paths, no values,
/// no secrets.
#[derive(Debug, Clone, Serialize)]
pub struct RecordingRecoveryHealthResponse {
    pub state: String,
    pub current_revision: u64,
    pub database_revision: u64,
    pub recovery_lag: u64,
    pub last_verified_checkpoint_revision: u64,
    pub journal_bytes: u64,
    pub last_error: Option<String>,
    pub storage_placement: String,
}

impl From<tuliprox_repository::recording_repository::RecoveryHealth> for RecordingRecoveryHealthResponse {
    fn from(health: tuliprox_repository::recording_repository::RecoveryHealth) -> Self {
        use tuliprox_repository::recording_repository::{
            RecoveryErrorClass, RecoveryRepositoryState, RecoveryStoragePlacement,
        };
        Self {
            state: match health.state {
                RecoveryRepositoryState::Healthy => "healthy",
                RecoveryRepositoryState::RepairRequired => "repair_required",
                RecoveryRepositoryState::Rebuilding => "rebuilding",
            }
            .to_string(),
            current_revision: health.current_revision,
            database_revision: health.database_revision,
            recovery_lag: health.recovery_lag,
            last_verified_checkpoint_revision: health.last_verified_checkpoint_revision,
            journal_bytes: health.journal_bytes,
            last_error: health.last_error.map(|class| {
                match class {
                    RecoveryErrorClass::Io => "io",
                    RecoveryErrorClass::Corruption => "corruption",
                    RecoveryErrorClass::SchemaMismatch => "schema_mismatch",
                    RecoveryErrorClass::MigrationFailed => "migration_failed",
                    RecoveryErrorClass::ForkedHistory => "forked_history",
                    RecoveryErrorClass::DatabaseAhead => "database_ahead",
                    RecoveryErrorClass::UncertainWrite => "uncertain_write",
                    RecoveryErrorClass::PublishFailed => "publish_failed",
                }
                .to_string()
            }),
            storage_placement: match health.storage_placement {
                RecoveryStoragePlacement::SameFilesystem => "same_filesystem",
                RecoveryStoragePlacement::DistinctFilesystem => "distinct_filesystem",
                RecoveryStoragePlacement::Unknown => "unknown",
            }
            .to_string(),
        }
    }
}

fn rule_error_response(err: &crate::api::model::recording_rule_service::RuleServiceError) -> axum::response::Response {
    use crate::api::model::recording_rule_service::RuleServiceError;
    let status = match err {
        RuleServiceError::Forbidden
        | RuleServiceError::SharedManagementNotAdministrator
        | RuleServiceError::NotOwner => StatusCode::FORBIDDEN,
        RuleServiceError::InvalidRule
        | RuleServiceError::InvalidFuture
        | RuleServiceError::LimitExceeded
        | RuleServiceError::Unsupported { .. } => StatusCode::BAD_REQUEST,
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

fn can_write_rules(claims: &shared::model::Claims) -> bool { claims.permissions.contains(Permission::RecordingManage) }

fn is_admin(claims: &shared::model::Claims) -> bool { claims.is_admin() }

async fn resolve_recording_source(
    app_state: &Arc<AppState>,
    target_name: &str,
    virtual_id: &mut String,
    input_name: &mut String,
    cluster: XtreamCluster,
    epg_channel_id: Option<&str>,
    channel_name: Option<&str>,
) -> Option<crate::api::endpoints::v1_api_playlist::ResolvedRecordingSource> {
    // A missing playlist id is resolved from the separately submitted EPG id.
    // Numeric EPG ids must not select a different playlist row by accident.
    let epg_id = if virtual_id.trim().is_empty() {
        Some(epg_channel_id.filter(|id| !id.trim().is_empty())?)
    } else if recording_virtual_id(virtual_id).is_none() {
        Some(virtual_id.as_str())
    } else {
        None
    };
    let mut resolved = if let Some(epg_id) = epg_id {
        if cluster != XtreamCluster::Live {
            return None;
        }
        let resolved = crate::api::endpoints::v1_api_playlist::resolve_target_live_recording_source_by_epg_channel(
            &app_state.app_config,
            target_name,
            input_name,
            epg_id,
            channel_name,
        )
        .await?;
        // The EPG lookup already checked the input's scope.
        accept_resolved_recording_source(virtual_id, input_name, &resolved).then_some(resolved)?
    } else {
        let virtual_id_value = recording_virtual_id(virtual_id)?;
        let resolved = crate::api::endpoints::v1_api_playlist::resolve_target_recording_source(
            &app_state.app_config,
            target_name,
            input_name,
            virtual_id_value,
            cluster,
        )
        .await?;
        accept_resolved_recording_source(virtual_id, input_name, &resolved).then_some(resolved)?
    };

    if let Some(target) = app_state.app_config.sources.load().get_target_by_name(target_name) {
        let classified = tuliprox_repository::target_stream_item::classify_virtual_id(
            &app_state.app_config,
            &app_state.playlists,
            target,
            shared::model::VirtualId::new(resolved.virtual_id),
        )
        .await;
        if matches!(classified, tuliprox_repository::target_stream_item::ResolvedStreamTarget::Group(_)) {
            resolved.is_group = true;
            resolved.provider_id = None;
        }
    }
    Some(resolved)
}

pub(crate) async fn load_target_playlist_item_for_evaluation(
    app_state: &AppState,
    target: &tuliprox_core::model::ConfigTarget,
    virtual_id: u32,
    cluster: Option<XtreamCluster>,
) -> Option<(shared::model::PlaylistItemType, shared::model::PlaylistItem)> {
    let classified = tuliprox_repository::target_stream_item::classify_virtual_id(
        &app_state.app_config,
        &app_state.playlists,
        target,
        shared::model::VirtualId::new(virtual_id),
    )
    .await;

    match classified {
        tuliprox_repository::target_stream_item::ResolvedStreamTarget::Member { ref group, index } => {
            let member_type = group.members[index].item_type;
            let group_item = crate::api::api_utils::failover_group::load_group_playlist_item(
                &app_state.app_config,
                &app_state.playlists,
                target,
                group.group_vid.get(),
            )
            .await?;
            Some((member_type, group_item))
        }
        tuliprox_repository::target_stream_item::ResolvedStreamTarget::Group(_) => {
            let group_item = crate::api::api_utils::failover_group::load_group_playlist_item(
                &app_state.app_config,
                &app_state.playlists,
                target,
                virtual_id,
            )
            .await?;
            let item_type = group_item.header.item_type;
            Some((item_type, group_item))
        }
        tuliprox_repository::target_stream_item::ResolvedStreamTarget::Item => {
            let item = tuliprox_repository::load_target_playlist_item(
                virtual_id,
                &app_state.app_config,
                &app_state.playlists,
                target,
                cluster,
            )
            .await?;
            Some((item.item_type(), item.to_playlist_item()))
        }
    }
}

pub async fn evaluate_api_user_target_source(
    app_state: &AppState,
    user: &ProxyUserCredentials,
    target: &ConfigTarget,
    target_id: &str,
    virtual_id: u32,
    cluster: Option<XtreamCluster>,
) -> bool {
    resolve_api_user_source_input_name(app_state, user, target, target_id, virtual_id, cluster).await.is_some()
}

pub async fn resolve_api_user_source_input_name(
    app_state: &AppState,
    user: &ProxyUserCredentials,
    target: &ConfigTarget,
    target_id: &str,
    virtual_id: u32,
    cluster: Option<XtreamCluster>,
) -> Option<String> {
    if user.permission_denied(&app_state.app_config) {
        return None;
    }
    if target.name != target_id && target_id.parse::<u16>() != Ok(target.id) {
        return None;
    }
    if user.t_filter.is_none() {
        if let Some(cluster) = cluster {
            if !user.allows_cluster(cluster) {
                return None;
            }
        }
    }
    let (item_type, judge_item) =
        load_target_playlist_item_for_evaluation(app_state, target, virtual_id, cluster).await?;
    if !user.allows_item_type(item_type) {
        return None;
    }
    if !crate::api::endpoints::hls_api::user_allows_playlist_filter(user, &judge_item) {
        return None;
    }
    Some(judge_item.header.input_name.to_string())
}

pub async fn authorize_api_user_source(
    app_state: &AppState,
    claims: &shared::model::Claims,
    target_id: &str,
    virtual_id: u32,
    cluster: Option<XtreamCluster>,
) -> Result<Option<String>, ServiceError> {
    let Some(subject_id) = claims.subject_id.as_ref() else {
        return Ok(None);
    };
    if !subject_id.is_api() {
        return Ok(None);
    }

    let Some((user, target)) = app_state.app_config.get_target_for_username(&claims.username) else {
        return Err(ServiceError::InvalidSource);
    };

    resolve_api_user_source_input_name(app_state, &user, &target, target_id, virtual_id, cluster)
        .await
        .ok_or(ServiceError::InvalidSource)
        .map(Some)
}

#[cfg(test)]
pub async fn check_recording_visibility(
    app_state: &AppState,
    meta: &shared::model::recording::RecordingMetadata,
    claims: &shared::model::Claims,
    subject_id: &UserId,
) -> bool {
    check_recording_visibility_with_context(app_state, meta, claims, subject_id, None).await
}

pub async fn check_recording_visibility_with_context(
    app_state: &AppState,
    meta: &shared::model::recording::RecordingMetadata,
    claims: &shared::model::Claims,
    subject_id: &UserId,
    visibility: Option<&AppRecordingVisibility<'_>>,
) -> bool {
    // The generic owner/visibility rule first; API users additionally see shared recordings only from their own
    // target and inside their content filter.
    if !crate::api::model::recording::recording_ws::task_visible_to(meta, claims, subject_id) {
        return false;
    }
    if !subject_id.is_api() || meta.owner_id() == subject_id {
        return true;
    }
    let user_target = if let Some(vis) = visibility {
        vis.get_api_user_target(&claims.username)
    } else {
        app_state.app_config.get_target_for_username(&claims.username)
    };
    let Some((user, target)) = user_target else {
        return false;
    };
    let Ok(vid) = meta.source.virtual_id.parse::<u32>() else {
        return false;
    };
    // The item cache is keyed by (vid, cluster) only, so reject other targets before consulting it.
    let source_target_id = meta.source.target_id.as_str();
    if !app_state.app_config.sources.load().target_ref_is(&target, source_target_id) {
        return false;
    }
    let cluster = Some(meta.source.cluster);
    if let Some(vis) = visibility {
        vis.evaluate_item_visibility(&user, &target, source_target_id, vid, cluster).await
    } else {
        evaluate_api_user_target_source(app_state, &user, &target, source_target_id, vid, cluster).await
    }
}

type CachedApiUserTarget = Option<(String, Option<(Arc<ProxyUserCredentials>, Arc<ConfigTarget>)>)>;
type ItemVisibilityCache = std::collections::HashMap<(u32, Option<XtreamCluster>), bool>;

pub struct AppRecordingVisibility<'a> {
    pub app_state: &'a AppState,
    cached_api_user_target: std::sync::Mutex<CachedApiUserTarget>,
    item_cache: std::sync::Mutex<ItemVisibilityCache>,
}

impl<'a> AppRecordingVisibility<'a> {
    #[must_use]
    pub fn new(app_state: &'a AppState) -> Self {
        Self {
            app_state,
            cached_api_user_target: std::sync::Mutex::new(None),
            item_cache: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    fn get_api_user_target(&self, username: &str) -> Option<(Arc<ProxyUserCredentials>, Arc<ConfigTarget>)> {
        let mut guard = self.cached_api_user_target.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((cached_username, cached)) = guard.as_ref() {
            if cached_username == username {
                return cached.clone();
            }
        }
        let resolved = self.app_state.app_config.get_target_for_username(username);
        *guard = Some((username.to_string(), resolved.clone()));
        resolved
    }

    fn get_cached_item(&self, key: (u32, Option<XtreamCluster>)) -> Option<bool> {
        self.item_cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(&key).copied()
    }

    fn insert_cached_item(&self, key: (u32, Option<XtreamCluster>), allowed: bool) {
        self.item_cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(key, allowed);
    }

    async fn evaluate_item_visibility(
        &self,
        user: &ProxyUserCredentials,
        target: &ConfigTarget,
        source_target_id: &str,
        vid: u32,
        cluster: Option<XtreamCluster>,
    ) -> bool {
        if let Some(cached) = self.get_cached_item((vid, cluster)) {
            return cached;
        }
        let allowed =
            evaluate_api_user_target_source(self.app_state, user, target, source_target_id, vid, cluster).await;
        self.insert_cached_item((vid, cluster), allowed);
        allowed
    }
}

impl tuliprox_dvr::recording::recording_catalog_access::RecordingVisibilityPort for AppRecordingVisibility<'_> {
    fn is_visible<'b>(
        &'b self,
        meta: &'b shared::model::recording::RecordingMetadata,
        claims: &'b shared::model::Claims,
        subject_id: &'b UserId,
    ) -> futures::future::BoxFuture<'b, bool> {
        Box::pin(async move {
            check_recording_visibility_with_context(self.app_state, meta, claims, subject_id, Some(self)).await
        })
    }
}

pub(crate) async fn recording_snapshot_response(
    app_state: &AppState,
    claims: &shared::model::Claims,
) -> shared::model::ProtocolMessage {
    let visibility = AppRecordingVisibility::new(app_state);
    let snapshot = crate::api::model::recording::recording_ws::recording_snapshot(
        &app_state.recording.queue,
        claims,
        &app_state.app_config,
        Some(&visibility),
    )
    .await;
    shared::model::ProtocolMessage::RecordingSnapshotResponse {
        revision: snapshot.revision,
        available: snapshot.available,
        quota: snapshot.quota,
        tasks: snapshot.tasks,
    }
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
    let revision = app_state.recording.queue.revision.load(std::sync::atomic::Ordering::SeqCst);

    let mut visible_rules = Vec::new();
    if admin {
        visible_rules = rules;
    } else if !subject_id.is_api() {
        visible_rules = rules
            .into_iter()
            .filter(|rule| rule.visibility == RuleVisibility::Shared || &rule.owner_id == subject_id)
            .collect();
    } else {
        let vis = AppRecordingVisibility::new(&app_state);
        let user_target = vis.get_api_user_target(&claims.username);
        for rule in rules {
            if &rule.owner_id == subject_id {
                visible_rules.push(rule);
            } else if rule.visibility == RuleVisibility::Shared {
                let Some((user, target)) = user_target.as_ref() else {
                    continue;
                };
                let source_target_id = rule.source.target_id.as_str();
                if !app_state.app_config.sources.load().target_ref_is(target, source_target_id) {
                    continue;
                }
                let Ok(vid) = rule.source.virtual_id.parse::<u32>() else {
                    continue;
                };
                if vis.evaluate_item_visibility(user, target, source_target_id, vid, Some(XtreamCluster::Live)).await {
                    visible_rules.push(rule);
                }
            }
        }
    }

    if claims.is_admin() {
        Json(visible_rules.into_iter().map(|rule| RecordingRuleResponse { revision, rule }).collect::<Vec<_>>())
            .into_response()
    } else {
        let caller_sub = claims.subject_id.as_ref();
        let user_rules: Vec<shared::model::recording_rule::UserRecordingRuleDto> = visible_rules
            .into_iter()
            .map(|rule| {
                let is_own = caller_sub.is_some_and(|sub| sub == &rule.owner_id);
                shared::model::recording_rule::UserRecordingRuleDto::from_rule(&rule, revision, is_own)
            })
            .collect();
        Json(user_rules).into_response()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RecordingRuleResponse {
    /// Latest queue revision observed when the response was assembled.
    /// The frontend uses this to detect stale snapshots when polling.
    pub revision: u64,
    #[serde(flatten)]
    pub rule: RecordingRule,
}

async fn resolve_rule_source_and_apply(
    app_state: &Arc<AppState>,
    source: &mut RuleSource,
    channel_id: Option<&str>,
) -> Result<(), RuleServiceError> {
    let resolved = resolve_recording_source(
        app_state,
        &source.target_id,
        &mut source.virtual_id,
        &mut source.input_name,
        XtreamCluster::Live,
        channel_id,
        None,
    )
    .await
    .ok_or(RuleServiceError::InvalidRule)?;
    source.is_group = resolved.is_group;
    source.provider_id = resolved.provider_id;
    Ok(())
}

#[allow(clippy::result_large_err)]
async fn resolve_and_authorize_api_user_rule_source(
    app_state: &Arc<AppState>,
    claims: &shared::model::Claims,
    rule: &mut RecordingRule,
) -> Result<(), Response> {
    rule.source.input_name.clear();
    if resolve_rule_source_and_apply(app_state, &mut rule.source, rule.channel_id.as_deref()).await.is_err() {
        return Err(service_error_response(&ServiceError::InvalidSource));
    }
    let vid =
        rule.source.virtual_id.parse::<u32>().map_err(|_| service_error_response(&ServiceError::InvalidSource))?;
    match authorize_api_user_source(app_state, claims, &rule.source.target_id, vid, Some(XtreamCluster::Live)).await {
        Ok(Some(input_name)) => {
            rule.source.input_name = input_name;
            Ok(())
        }
        Ok(None) => Ok(()),
        Err(err) => Err(service_error_response(&err)),
    }
}

/// POST /api/v1/recording/rules
pub async fn create_recording_rule(
    State(app_state): State<Arc<AppState>>,
    AuthClaims(claims): AuthClaims,
    Json(body): Json<CreateRecordingRuleBody>,
) -> impl IntoResponse {
    let Some(owner_id) = claims.subject_id.clone() else {
        return error_response(StatusCode::UNAUTHORIZED, "recording_token_refresh_required");
    };
    if !can_write_rules(&claims) {
        return error_response(StatusCode::FORBIDDEN, "recording_forbidden");
    }

    let is_api = owner_id.is_api();
    let (api_user_opt, api_target_opt) = if is_api {
        let Some((u, t)) = app_state.app_config.get_target_for_username(&claims.username) else {
            return service_error_response(&ServiceError::InvalidSource);
        };
        (Some(u), Some(t))
    } else {
        (None, None)
    };

    let target_id = match body.target_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(tid) => {
            if let Some(target) = api_target_opt.as_ref() {
                if tid != target.name && tid != target.id.to_string() {
                    return service_error_response(&ServiceError::InvalidSource);
                }
                target.name.clone()
            } else {
                let sources = app_state.app_config.sources.load();
                let found_target = sources
                    .get_target_by_name(tid)
                    .cloned()
                    .or_else(|| tid.parse::<u16>().ok().and_then(|id| sources.get_target_by_id(id)));
                if let Some(target) = found_target {
                    target.name.clone()
                } else {
                    return service_error_response(&ServiceError::InvalidSource);
                }
            }
        }
        None => {
            if let Some(target) = api_target_opt.as_ref() {
                target.name.clone()
            } else {
                return rule_error_response(&RuleServiceError::InvalidRule);
            }
        }
    };

    let now = chrono::Utc::now().timestamp();
    let mut rule = recording_rule_from_create(owner_id, body, target_id, now);
    if is_api {
        if let Err(resp) = resolve_and_authorize_api_user_rule_source(&app_state, &claims, &mut rule).await {
            return resp;
        }
    } else if let Err(err) =
        resolve_rule_source_and_apply(&app_state, &mut rule.source, rule.channel_id.as_deref()).await
    {
        return rule_error_response(&err);
    }

    if let Err(err) = crate::api::model::recording_rule_service::validate_rule(&rule).and_then(|()| {
        crate::api::model::recording_rule_service::authorize_rule_action(
            can_write_rules(&claims),
            claims.permissions.contains(Permission::RecordingShare),
            claims.is_admin(),
            &rule.owner_id,
            &rule,
        )
    }) {
        return rule_error_response(&err);
    }

    let max_rules = api_user_opt.as_ref().map(|u| u.player_features.max_recording_rules);
    match recording_rule_repo(&app_state).create_with_owner_limit(rule, max_rules).await {
        Ok(rule) => {
            let _ = app_state.event_manager.send_event(EventMessage::RecordingRulesChanged);
            let revision = app_state.recording.queue.revision.load(std::sync::atomic::Ordering::SeqCst);
            if claims.is_admin() {
                Json(RecordingRuleResponse { revision, rule }).into_response()
            } else {
                Json(shared::model::recording_rule::UserRecordingRuleDto::from_rule(&rule, revision, true))
                    .into_response()
            }
        }
        Err(tuliprox_repository::recording_rule_repository::CreateRuleError::LimitExceeded) => {
            rule_error_response(&RuleServiceError::LimitExceeded)
        }
        Err(tuliprox_repository::recording_rule_repository::CreateRuleError::Io(_)) => {
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "recording_persistence_failed")
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateRecordingRuleBody {
    #[serde(default)]
    pub target_id: Option<String>,
    pub virtual_id: String,
    #[serde(default)]
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

fn recording_rule_from_create(
    owner_id: UserId,
    body: CreateRecordingRuleBody,
    target_id: String,
    now: i64,
) -> RecordingRule {
    RecordingRule {
        id: format!("rule-{}-{}", now, shared::utils::generate_random_string(8)),
        owner_id,
        visibility: body.visibility.unwrap_or_default(),
        enabled: true,
        source: RuleSource::new(target_id, body.virtual_id, body.input_name),
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
    let channel_id_changed = body.channel_id.is_some() || body.clear_channel_id;
    if let Err(err) = authorize_and_apply_recording_rule_edit(&claims, &mut rule, body, chrono::Utc::now().timestamp())
    {
        return match err {
            EditRuleError::MissingSubject => {
                error_response(StatusCode::UNAUTHORIZED, "recording_token_refresh_required")
            }
            EditRuleError::Rule(err) => rule_error_response(&err),
        };
    }
    if channel_id_changed {
        if claims.subject_id.as_ref().is_some_and(shared::model::UserId::is_api) {
            if let Err(resp) = resolve_and_authorize_api_user_rule_source(&app_state, &claims, &mut rule).await {
                return resp;
            }
        } else {
            let _ = resolve_rule_source_and_apply(&app_state, &mut rule.source, rule.channel_id.as_deref()).await;
        }
    }
    match repo.update(rule).await {
        Ok(Some(rule)) => {
            let _ = app_state.event_manager.send_event(EventMessage::RecordingRulesChanged);
            let revision = app_state.recording.queue.revision.load(std::sync::atomic::Ordering::SeqCst);
            if claims.is_admin() {
                Json(RecordingRuleResponse { revision, rule }).into_response()
            } else {
                let is_own = claims.subject_id.as_ref().is_some_and(|sub| sub == &rule.owner_id);
                Json(shared::model::recording_rule::UserRecordingRuleDto::from_rule(&rule, revision, is_own))
                    .into_response()
            }
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
        claims.permissions.contains(Permission::RecordingShare),
        claims.is_admin(),
        subject_id,
        rule,
    )
    .map_err(EditRuleError::Rule)?;
    apply_recording_rule_edit(rule, body, now);
    crate::api::model::recording_rule_service::validate_rule(rule).map_err(EditRuleError::Rule)?;
    crate::api::model::recording_rule_service::authorize_rule_action(
        can_write_rules(claims),
        claims.permissions.contains(Permission::RecordingShare),
        claims.is_admin(),
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
    if claims.subject_id.is_none() {
        return error_response(StatusCode::UNAUTHORIZED, "recording_token_refresh_required");
    }
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
        claims.permissions.contains(Permission::RecordingShare),
        claims.is_admin(),
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
    let service = RecordingService::new(app_state.recording.queue.clone(), app_state.app_config.clone());
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
    // Any recording permission is enough to ask whether the DVR is usable;
    // the answer carries no recording data.
    let recording_principal = [
        Permission::RecordingRead,
        Permission::RecordingCreate,
        Permission::RecordingManage,
        Permission::RecordingDelete,
    ]
    .into_iter()
    .any(|permission| claims.permissions.contains(permission));
    if !recording_principal {
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
    // A request id is the caller's own library entry. A materialization id
    // is the physical recording behind it, which several entries may
    // share, so those routes are administrator-only and are the only ones
    // that can stop or destroy work another user is still attached to.
    let recording_routes = Router::new()
        .route("/requests", post(create_recording_request))
        .route("/requests/{id}", patch(edit_recording_task).delete(remove_recording_task))
        .route("/requests/{id}/cancel", post(cancel_recording_task))
        .route("/materializations/{id}/pause", post(pause_recording_task))
        .route("/materializations/{id}/resume", post(resume_recording_task))
        .route("/materializations/{id}/retry", post(retry_recording_task))
        .route("/materializations/{id}/restart", post(restart_recording_task))
        .route("/materializations/{id}", axum::routing::delete(delete_recording_task))
        .route("/conflicts/preview", post(preview_recording_conflicts))
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
    use crate::api::model::RecordingTask;
    use futures::future::BoxFuture;
    use serde_json::json;
    use shared::model::{
        recording::{RecordingMetadata, RecordingOwner, RecordingSource, RecordingVisibility},
        RecordingKind,
    };
    use std::time::Duration;
    use tokio::sync::Notify;
    use tuliprox_core::model::ProviderHandle;
    use tuliprox_dvr::recording::recording_capacity::{ProviderCapacity, RecordingCapacityPort};

    struct UnavailableCapacity {
        notify: Arc<Notify>,
    }

    impl UnavailableCapacity {
        fn new() -> Arc<Self> { Arc::new(Self { notify: Arc::new(Notify::new()) }) }
    }

    impl RecordingCapacityPort for UnavailableCapacity {
        fn capacities_for_input<'a>(&'a self, _input_name: &'a Arc<str>) -> BoxFuture<'a, Vec<ProviderCapacity>> {
            Box::pin(async { vec![(Arc::from("provider"), 1, 1)] })
        }

        fn acquire<'a>(&'a self, _input_name: &'a Arc<str>, _priority: i8) -> BoxFuture<'a, Option<ProviderHandle>> {
            Box::pin(async { None })
        }

        fn release(&self, _handle: Option<ProviderHandle>) -> BoxFuture<'_, ()> { Box::pin(async {}) }

        fn capacity_changed(&self) -> Arc<Notify> { Arc::clone(&self.notify) }

        fn is_input_in_maintenance(&self, _input_name: &str) -> bool { false }
    }

    fn edit_claims(subject_id: Option<UserId>, admin: bool) -> shared::model::Claims {
        shared::model::Claims {
            username: "alice".to_string(),
            iss: "tuliprox".to_string(),
            iat: 0,
            exp: 0,
            roles: if admin { shared::model::RoleSet::ADMIN } else { shared::model::RoleSet::new() },
            permissions: Permission::RecordingManage.into(),
            pwd_version: 0,
            subject_id,
            permission_schema_version: shared::model::CURRENT_PERMISSION_SCHEMA_VERSION,
        }
    }

    fn enabled_recording_state() -> Arc<AppState> {
        let recording_dto = shared::model::RecordingConfigDto { enabled: true, ..Default::default() };
        let recording_runtime = crate::model::RecordingConfig::from(&recording_dto);
        crate::api::model::create_test_app_state(crate::model::Config {
            video: Some(crate::model::VideoConfig {
                extensions: Vec::new(),
                web_search: None,
                recording: Some(recording_runtime),
            }),
            ..crate::model::Config::default()
        })
    }

    fn resumable_vod(state: shared::model::RecordingTaskState) -> RecordingTask {
        let recording_config = crate::model::RecordingConfig::from(&shared::model::RecordingConfigDto {
            enabled: true,
            ..Default::default()
        });
        let metadata = RecordingMetadata::new_media(
            RecordingOwner::User(UserId::from("web:alice")),
            RecordingVisibility::Private,
            RecordingSource::new("target", "42", "provider"),
            "Film".to_string(),
        );
        let mut task = RecordingTask::new(
            RecordingKind::Vod,
            "https://example.com/film.mp4",
            "film.mp4",
            &recording_config,
            Some(Arc::from("provider")),
            recording_config.priority,
            metadata,
        )
        .expect("valid VOD task");
        task.uuid = "vod".to_string();
        task.state = state;
        task.paused = state == shared::model::RecordingTaskState::Paused;
        task.finished = state.is_terminal();
        task
    }

    async fn wait_until_worker_owns_runnable_task(state: &AppState) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let running = state.recording.queue.workers_running().await;
                let runnable = state.recording.queue.active.read().await.first().is_some_and(|task| {
                    matches!(
                        task.state,
                        shared::model::RecordingTaskState::Running
                            | shared::model::RecordingTaskState::WaitingForCapacity
                    ) && !task.paused
                });
                if running && runnable {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("recording worker did not start");
    }

    #[tokio::test]
    async fn resume_command_restarts_the_worker_for_a_paused_vod() {
        let mut state = enabled_recording_state();
        Arc::get_mut(&mut state).expect("unique app state").recording.capacity = UnavailableCapacity::new();
        *state.recording.queue.active.write().await = vec![resumable_vod(shared::model::RecordingTaskState::Paused)];

        let response = resume_recording_task(
            axum::extract::Path("vod".to_string()),
            State(Arc::clone(&state)),
            AuthClaims(edit_claims(Some(UserId::from("web:alice")), true)),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        wait_until_worker_owns_runnable_task(&state).await;
    }

    #[tokio::test]
    async fn retry_command_restarts_the_worker_for_a_cancelled_vod() {
        let mut state = enabled_recording_state();
        Arc::get_mut(&mut state).expect("unique app state").recording.capacity = UnavailableCapacity::new();
        state.recording.queue.finished.write().await.push(resumable_vod(shared::model::RecordingTaskState::Cancelled));

        let response = retry_recording_task(
            axum::extract::Path("vod".to_string()),
            State(Arc::clone(&state)),
            AuthClaims(edit_claims(Some(UserId::from("web:alice")), true)),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        wait_until_worker_owns_runnable_task(&state).await;
    }

    /// Recording enabled under `dir`, with target `1` fed by input `input-a`,
    /// so the service can resolve a VOD request to its source.
    fn media_request_state(dir: &std::path::Path) -> Arc<AppState> {
        let mut recording = crate::model::RecordingConfig::from(&shared::model::RecordingConfigDto {
            enabled: true,
            ..Default::default()
        });
        recording.directory = dir.to_string_lossy().into_owned();
        let mut state = crate::api::model::create_test_app_state(crate::model::Config {
            video: Some(crate::model::VideoConfig {
                extensions: Vec::new(),
                web_search: None,
                recording: Some(recording),
            }),
            ..crate::model::Config::default()
        });
        let input = Arc::new(tuliprox_core::model::ConfigInput { id: 7, name: "input-a".into(), ..Default::default() });
        let target = Arc::new(tuliprox_core::model::ConfigTarget {
            managed_channels: Vec::new(),
            id: 11,
            enabled: true,
            name: "1".to_string(),
            options: None,
            sort: None,
            filter: tuliprox_core::model::StagedFilter::default(),
            output: vec![],
            rename: None,
            mapping_ids: None,
            mapping: Arc::default(),
            favourites: None,
            processing_order: shared::model::ProcessingOrder::default(),
            curation: None,
            execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
            watch: None,
            use_memory_cache: false,
        });
        state.app_config.sources.store(Arc::new(tuliprox_core::model::SourcesConfig {
            inputs: vec![input],
            sources: vec![tuliprox_core::model::ConfigSource { inputs: vec!["input-a".into()], targets: vec![target] }],
            ..tuliprox_core::model::SourcesConfig::default()
        }));
        Arc::get_mut(&mut state).expect("unique app state").recording.capacity = UnavailableCapacity::new();
        state
    }

    async fn live_request_state(
        dir: &std::path::Path,
        output: shared::model::TargetType,
    ) -> Result<Arc<AppState>, Box<dyn std::error::Error>> {
        use shared::model::{PlaylistItem, PlaylistItemHeader, PlaylistItemType, VirtualId};
        use tuliprox_core::model::{M3uTargetOutput, TargetOutput, XtreamTargetOutput};
        use tuliprox_repository::{
            ensure_target_storage_path, ensure_xtream_storage_path, m3u_get_file_path_for_db, xtream_get_file_path,
            BPlusTree,
        };

        let state = media_request_state(dir);
        let mut config = (*state.app_config.config.load_full()).clone();
        config.storage_dir = dir.to_string_lossy().into_owned();
        state.app_config.config.store(Arc::new(config));
        let mut sources = (*state.app_config.sources.load_full()).clone();
        let mut target = (*sources.sources[0].targets[0]).clone();
        target.output = vec![match output {
            shared::model::TargetType::Xtream => {
                TargetOutput::Xtream(XtreamTargetOutput::from(&shared::model::XtreamTargetOutputDto::default()))
            }
            _ => TargetOutput::M3u(M3uTargetOutput::from(&shared::model::M3uTargetOutputDto::default())),
        }];
        for (id, name) in [(8, "input-b"), (9, "foreign")] {
            sources.inputs.push(Arc::new(tuliprox_core::model::ConfigInput {
                id,
                name: name.into(),
                ..Default::default()
            }));
        }
        sources.sources[0].inputs.push("input-b".into());
        sources.sources[0].targets[0] = Arc::new(target.clone());
        state.app_config.sources.store(Arc::new(sources));
        let storage = ensure_target_storage_path(&state.app_config.config.load(), &target.name).await?;
        let items = [
            (42, "input-a", "Channel HD", "43"),
            (43, "input-b", "Channel 4K", "43"),
            (44, "foreign", "Foreign", "43"),
            (45, "input-a", "Legacy", "shared.epg"),
        ]
        .map(|(id, input_name, title, epg_id)| PlaylistItem {
            header: PlaylistItemHeader {
                virtual_id: VirtualId::new(id),
                input_name: input_name.into(),
                name: title.into(),
                title: title.into(),
                group: "News".into(),
                epg_channel_id: Some(epg_id.into()),
                url: "http://upstream.example/live.ts".into(),
                item_type: PlaylistItemType::Live,
                xtream_cluster: XtreamCluster::Live,
                ..Default::default()
            },
        });
        if output == shared::model::TargetType::Xtream {
            let storage = ensure_xtream_storage_path(&state.app_config.config.load(), &target.name).await?;
            let path = xtream_get_file_path(&storage, XtreamCluster::Live);
            std::fs::create_dir_all(path.parent().ok_or("database parent missing")?)?;
            let mut tree = BPlusTree::new();
            for item in &items {
                tree.insert(item.header.virtual_id, shared::model::XtreamPlaylistItem::from(item));
            }
            tree.store(&path)?;
        } else {
            let path = m3u_get_file_path_for_db(&storage);
            std::fs::create_dir_all(path.parent().ok_or("database parent missing")?)?;
            let mut tree = BPlusTree::new();
            for item in &items {
                tree.insert(item.header.virtual_id, shared::model::M3uPlaylistItem::from(item));
            }
            tree.store(&path)?;
        }
        Ok(state)
    }

    async fn post_live(
        state: &Arc<AppState>,
        virtual_id: &str,
        input_name: &str,
    ) -> Result<axum::response::Response, Box<dyn std::error::Error>> {
        let start = chrono::Utc::now().timestamp() + 3_600;
        let body = serde_json::from_value(json!({
            "source": {"target_id": "1", "virtual_id": virtual_id, "input_name": input_name, "cluster": "Live"},
            "program_title": "News",
            "program_start": start,
            "program_end": start + 120,
            "visibility": "private",
            "channel_id": "43",
            "channel_name": "Channel HD"
        }))?;
        Ok(create_recording_request(
            State(Arc::clone(state)),
            AuthClaims(creator_claims("alice", Permission::RecordingCreate.into())),
            HeaderMap::new(),
            Json(body),
        )
        .await
        .into_response())
    }

    #[tokio::test]
    async fn epg_recording_request_resolves_missing_input_from_the_exact_playlist_channel(
    ) -> Result<(), Box<dyn std::error::Error>> {
        for output in [shared::model::TargetType::Xtream, shared::model::TargetType::M3u] {
            let dir = tempfile::tempdir()?;
            let state = live_request_state(dir.path(), output).await?;
            // The title points at HD, but the selected playlist row is the 4K feed.
            let response = post_live(&state, "43", "").await?;
            assert_eq!(response.status(), StatusCode::NO_CONTENT, "{output:?}");
            let (_, tasks) = state.recording.queue.committed_snapshot().await;
            assert_eq!(tasks.len(), 1);
            let task = &tasks[0];
            assert_eq!(task.recording.source.virtual_id, "43");
            assert_eq!(task.recording.source.input_name, "input-b");
            assert_eq!(task.recording.group.as_deref(), Some("News"));
            assert_eq!(task.state, shared::model::RecordingTaskState::Scheduled);
        }
        Ok(())
    }

    #[tokio::test]
    async fn explorer_and_legacy_epg_recording_requests_still_resolve() -> Result<(), Box<dyn std::error::Error>> {
        for output in [shared::model::TargetType::Xtream, shared::model::TargetType::M3u] {
            for (id, input, expected) in [("42", "input-a", "42"), ("shared.epg", "", "45")] {
                let dir = tempfile::tempdir()?;
                let state = live_request_state(dir.path(), output).await?;
                assert_eq!(post_live(&state, id, input).await?.status(), StatusCode::NO_CONTENT);
                let (_, tasks) = state.recording.queue.committed_snapshot().await;
                assert_eq!(tasks[0].recording.source.virtual_id, expected);
                assert_eq!(tasks[0].recording.source.input_name, "input-a");
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn full_epg_recording_resolves_numeric_epg_id_without_using_it_as_a_playlist_id(
    ) -> Result<(), Box<dyn std::error::Error>> {
        for output in [shared::model::TargetType::Xtream, shared::model::TargetType::M3u] {
            let dir = tempfile::tempdir()?;
            let state = live_request_state(dir.path(), output).await?;
            // EPG channel 43 labels the HD feed, whose playlist id is 42.
            // Playlist row 43 is a different stream with the same EPG id.
            let response = post_live(&state, "", "").await?;
            assert_eq!(response.status(), StatusCode::NO_CONTENT, "{output:?}");
            let (_, tasks) = state.recording.queue.committed_snapshot().await;
            assert_eq!(tasks.len(), 1);
            assert_eq!(tasks[0].recording.source.virtual_id, "42");
            assert_eq!(tasks[0].recording.source.input_name, "input-a");
        }
        Ok(())
    }

    #[tokio::test]
    async fn epg_recording_source_filters_input_before_title_hint() -> Result<(), Box<dyn std::error::Error>> {
        for output in [shared::model::TargetType::Xtream, shared::model::TargetType::M3u] {
            let dir = tempfile::tempdir()?;
            let state = live_request_state(dir.path(), output).await?;
            for (input, title, expected) in [("input-a", "Channel 4K", "42"), ("input-b", "Channel HD", "43")] {
                let mut virtual_id = String::new();
                let mut input_name = input.to_string();
                let resolved = resolve_recording_source(
                    &state,
                    "1",
                    &mut virtual_id,
                    &mut input_name,
                    XtreamCluster::Live,
                    Some("43"),
                    Some(title),
                )
                .await;
                assert!(resolved.is_some(), "{output:?}: {input}/{title}");
                assert_eq!(virtual_id, expected);
                assert_eq!(input_name, input);
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn epg_recording_rules_resolve_both_playlist_and_explicit_epg_identity(
    ) -> Result<(), Box<dyn std::error::Error>> {
        for output in [shared::model::TargetType::Xtream, shared::model::TargetType::M3u] {
            for (id, input, expected) in
                [("43", "", "43"), ("", "", "42"), ("", "input-a", "42"), ("", "input-b", "43")]
            {
                let dir = tempfile::tempdir()?;
                let state = live_request_state(dir.path(), output).await?;
                let body = serde_json::from_value(json!({
                    "target_id": "1", "virtual_id": id, "input_name": input, "channel_id": "43",
                    "body": {"kind": "weekly_timeslot", "weekday": 3, "local_start_time": "20:00",
                             "duration_secs": 120, "timezone": "America/Toronto"}
                }))?;
                let response = create_recording_rule(
                    State(Arc::clone(&state)),
                    AuthClaims(creator_claims("alice", Permission::RecordingManage.into())),
                    Json(body),
                )
                .await
                .into_response();
                assert_eq!(response.status(), StatusCode::OK, "{output:?}: {id}");
                let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
                let rule: RecordingRule = serde_json::from_slice(&body)?;
                assert_eq!(rule.source.virtual_id, expected);
                assert_eq!(rule.source.input_name, if expected == "43" { "input-b" } else { "input-a" });
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn recording_requests_reject_conflicting_unknown_and_out_of_scope_sources(
    ) -> Result<(), Box<dyn std::error::Error>> {
        for output in [shared::model::TargetType::Xtream, shared::model::TargetType::M3u] {
            for (id, input) in
                [("43", "input-a"), ("999", ""), ("44", ""), ("42", "unknown"), ("", "unknown"), ("", "foreign")]
            {
                let dir = tempfile::tempdir()?;
                let state = live_request_state(dir.path(), output).await?;
                let response = post_live(&state, id, input).await?;
                assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{output:?}: {id}/{input}");
                let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
                assert_eq!(serde_json::from_slice::<serde_json::Value>(&body)?["error"], "recording_invalid_source");
                assert_eq!(tasks_owned_by(&state, "web:alice").await, 0);
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn conflict_preview_resolves_epg_rows_without_playlist_id_or_input() -> Result<(), Box<dyn std::error::Error>>
    {
        for output in [shared::model::TargetType::Xtream, shared::model::TargetType::M3u] {
            let dir = tempfile::tempdir()?;
            let state = live_request_state(dir.path(), output).await?;
            let start = chrono::Utc::now().timestamp() + 3_600;
            for (virtual_id, input_name, channel_id, expected) in [
                ("", "", Some("43"), StatusCode::OK),
                ("", "input-a", Some("43"), StatusCode::OK),
                ("", "input-b", Some("43"), StatusCode::OK),
                ("shared.epg", "input-a", None, StatusCode::OK),
                ("", "", None, StatusCode::BAD_REQUEST),
                ("", "input-a", None, StatusCode::BAD_REQUEST),
                ("999", "input-a", None, StatusCode::BAD_REQUEST),
                ("43", "input-a", None, StatusCode::BAD_REQUEST),
            ] {
                let body = serde_json::from_value(json!({
                    "source": {"target_name": "1", "virtual_id": virtual_id, "input_name": input_name,
                               "channel_id": channel_id, "channel_name": "Channel HD"},
                    "candidate": {"padded_start": start, "padded_end": start + 120}
                }))?;
                let response = preview_recording_conflicts(
                    AuthClaims(creator_claims("alice", Permission::RecordingCreate.into())),
                    State(Arc::clone(&state)),
                    Json(body),
                )
                .await;
                assert_eq!(response.status(), expected, "{output:?}: {channel_id:?}");
            }
        }
        Ok(())
    }

    fn vod_request(
    ) -> (CreateRecordingRequest, RecordingSourceRequest, crate::api::endpoints::v1_api_playlist::ResolvedRecordingSource)
    {
        let source = RecordingSourceRequest {
            target_id: "1".to_string(),
            virtual_id: "77".to_string(),
            cluster: XtreamCluster::Video,
            input_name: "input-a".to_string(),
        };
        let body = CreateRecordingRequest {
            source: source.clone(),
            program_title: "The Film".to_string(),
            program_start: None,
            program_end: None,
            pre_roll_secs: None,
            post_roll_secs: None,
            visibility: RecordingVisibility::Private,
            channel_id: None,
            channel_name: None,
            epg: None,
        };
        let resolved = crate::api::endpoints::v1_api_playlist::ResolvedRecordingSource {
            virtual_id: 77,
            input_name: "input-a".to_string(),
            title: "The Film".to_string(),
            group: None,
            series_name: None,
            extension: Some("mp4".to_string()),
            downloadable: true,
            provider_id: None,
            is_group: false,
        };
        (body, source, resolved)
    }

    fn creator_claims(user: &str, permissions: shared::model::permission::PermissionSet) -> shared::model::Claims {
        shared::model::Claims {
            username: user.to_string(),
            subject_id: Some(UserId::from(format!("web:{user}"))),
            permissions,
            ..edit_claims(None, true)
        }
    }

    async fn post_vod(state: &Arc<AppState>, claims: &shared::model::Claims) -> axum::response::Response {
        let (body, source, resolved) = vod_request();
        create_http_recording_task(state, claims, body, source, resolved, None).await
    }

    async fn tasks_owned_by(state: &AppState, owner: &str) -> usize {
        let (_, tasks) = state.recording.queue.committed_snapshot().await;
        tasks.iter().filter(|task| task.owner_id() == &UserId::from(owner)).count()
    }

    #[tokio::test]
    async fn a_vod_request_answers_no_content_and_queues_one_transfer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = media_request_state(dir.path());

        let response = post_vod(&state, &creator_claims("alice", Permission::RecordingCreate.into())).await;

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(tasks_owned_by(&state, "web:alice").await, 1);
    }

    #[tokio::test]
    async fn a_vod_request_needs_recording_create() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = media_request_state(dir.path());

        let response = post_vod(&state, &creator_claims("alice", Permission::RecordingManage.into())).await;

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(tasks_owned_by(&state, "web:alice").await, 0);
    }

    #[tokio::test]
    async fn another_users_vod_request_is_never_answered_with_the_first_users_entry() {
        // The duplicate check is per owner: another user's request for the
        // same film is a new entry, and nobody is handed someone else's.
        let dir = tempfile::tempdir().expect("tempdir");
        let state = media_request_state(dir.path());
        let alice = creator_claims("alice", Permission::RecordingCreate.into());
        let bob = creator_claims("bob", Permission::RecordingCreate.into());

        assert_eq!(post_vod(&state, &alice).await.status(), StatusCode::NO_CONTENT);
        let response = post_vod(&state, &bob).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("body");
        assert!(body.is_empty(), "a command carries no body, least of all another user's entry");
        assert_eq!(tasks_owned_by(&state, "web:bob").await, 1);

        let repeat = post_vod(&state, &alice).await;
        assert_eq!(repeat.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(repeat.into_body(), usize::MAX).await.expect("body");
        assert_eq!(body.as_ref(), br#"{"error":"recording_duplicate"}"#);
        assert_eq!(tasks_owned_by(&state, "web:alice").await, 1);
    }

    #[tokio::test]
    async fn recording_availability_accepts_any_recording_permission() {
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

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn api_user_rules_permission_levels_matrix() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let state = live_request_state(dir.path(), shared::model::TargetType::Xtream).await?;

        let api_proxy_dto = shared::model::ApiProxyConfigDto {
            user: vec![shared::model::TargetUserDto {
                target: "1".to_string(),
                credentials: vec![
                    shared::model::ProxyUserCredentialsDto {
                        username: "user_none".to_string(),
                        password: "pass".to_string(),
                        ui_enabled: true,
                        player_features: Some(shared::model::PlayerFeaturesDto {
                            recording: Some(shared::model::PlayerRecordingLevel::None),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    shared::model::ProxyUserCredentialsDto {
                        username: "user_read".to_string(),
                        password: "pass".to_string(),
                        ui_enabled: true,
                        player_features: Some(shared::model::PlayerFeaturesDto {
                            recording: Some(shared::model::PlayerRecordingLevel::Read),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    shared::model::ProxyUserCredentialsDto {
                        username: "user_private".to_string(),
                        password: "pass".to_string(),
                        ui_enabled: true,
                        player_features: Some(shared::model::PlayerFeaturesDto {
                            recording: Some(shared::model::PlayerRecordingLevel::Private),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    shared::model::ProxyUserCredentialsDto {
                        username: "user_shared".to_string(),
                        password: "pass".to_string(),
                        ui_enabled: true,
                        player_features: Some(shared::model::PlayerFeaturesDto {
                            recording: Some(shared::model::PlayerRecordingLevel::Shared),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                ],
            }],
            ..Default::default()
        };
        state.app_config.api_proxy.store(Some(Arc::new(tuliprox_core::model::ApiProxyConfig::from(&api_proxy_dto))));

        let claims_none = test_api_claims("user_none", shared::model::PermissionSet::new());
        let claims_read = test_api_claims("user_read", Permission::RecordingRead);
        let claims_private = test_api_claims(
            "user_private",
            Permission::RecordingRead
                | Permission::RecordingCreate
                | Permission::RecordingManage
                | Permission::RecordingDelete,
        );
        let claims_shared = test_api_claims(
            "user_shared",
            Permission::RecordingRead
                | Permission::RecordingCreate
                | Permission::RecordingManage
                | Permission::RecordingDelete
                | Permission::RecordingShare,
        );

        let make_rule_body = |vis: Option<RuleVisibility>| CreateRecordingRuleBody {
            target_id: None, // defaults to caller target "1"
            virtual_id: "42".to_string(),
            input_name: String::new(),
            channel_id: None,
            visibility: vis,
            body: RuleBody::WeeklyTimeslot {
                weekday: 3,
                local_start_time: "20:00".to_string(),
                duration_secs: 3600,
                timezone: "UTC".to_string(),
            },
            pre_roll_secs: 0,
            post_roll_secs: 0,
        };

        // 1. None: list -> 403, create -> 403
        let res =
            list_recording_rules(State(Arc::clone(&state)), AuthClaims(claims_none.clone())).await.into_response();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let res = create_recording_rule(
            State(Arc::clone(&state)),
            AuthClaims(claims_none.clone()),
            Json(make_rule_body(Some(RuleVisibility::Private))),
        )
        .await
        .into_response();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);

        // 2. Read: list -> 200, create -> 403
        let res =
            list_recording_rules(State(Arc::clone(&state)), AuthClaims(claims_read.clone())).await.into_response();
        assert_eq!(res.status(), StatusCode::OK);
        let res = create_recording_rule(
            State(Arc::clone(&state)),
            AuthClaims(claims_read.clone()),
            Json(make_rule_body(Some(RuleVisibility::Private))),
        )
        .await
        .into_response();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);

        // 3. Private: create private -> 200, create shared -> 403
        let res = create_recording_rule(
            State(Arc::clone(&state)),
            AuthClaims(claims_private.clone()),
            Json(make_rule_body(Some(RuleVisibility::Shared))),
        )
        .await
        .into_response();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
        let json: serde_json::Value = serde_json::from_slice(&bytes)?;
        assert_eq!(json["error"], "recording_shared_not_administrator");

        let res = create_recording_rule(
            State(Arc::clone(&state)),
            AuthClaims(claims_private.clone()),
            Json(make_rule_body(Some(RuleVisibility::Private))),
        )
        .await
        .into_response();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
        let json: serde_json::Value = serde_json::from_slice(&bytes)?;
        let priv_rule_id = json["id"].as_str().unwrap().to_string();

        // Private: edit own private rule -> 200
        let edit_body = EditRecordingRuleBody { pre_roll_secs: Some(120), ..Default::default() };
        let res = edit_recording_rule(
            axum::extract::Path(priv_rule_id.clone()),
            State(Arc::clone(&state)),
            AuthClaims(claims_private.clone()),
            Json(edit_body),
        )
        .await
        .into_response();
        assert_eq!(res.status(), StatusCode::OK);

        // Private: edit own rule to shared -> 403
        let edit_shared = EditRecordingRuleBody { visibility: Some(RuleVisibility::Shared), ..Default::default() };
        let res = edit_recording_rule(
            axum::extract::Path(priv_rule_id.clone()),
            State(Arc::clone(&state)),
            AuthClaims(claims_private.clone()),
            Json(edit_shared),
        )
        .await
        .into_response();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);

        // Private: delete own rule -> 204
        let res = delete_recording_rule(
            axum::extract::Path(priv_rule_id),
            axum::extract::Query(DeleteRuleParams { future: Some("retain".to_string()) }),
            State(Arc::clone(&state)),
            AuthClaims(claims_private.clone()),
        )
        .await
        .into_response();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);

        // 4. Shared: create shared -> 200, edit -> 200, delete -> 204
        let res = create_recording_rule(
            State(Arc::clone(&state)),
            AuthClaims(claims_shared.clone()),
            Json(make_rule_body(Some(RuleVisibility::Shared))),
        )
        .await
        .into_response();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
        let json: serde_json::Value = serde_json::from_slice(&bytes)?;
        let shared_rule_id = json["id"].as_str().unwrap().to_string();

        let edit_body = EditRecordingRuleBody { pre_roll_secs: Some(60), ..Default::default() };
        let res = edit_recording_rule(
            axum::extract::Path(shared_rule_id.clone()),
            State(Arc::clone(&state)),
            AuthClaims(claims_shared.clone()),
            Json(edit_body),
        )
        .await
        .into_response();
        assert_eq!(res.status(), StatusCode::OK);

        let res = delete_recording_rule(
            axum::extract::Path(shared_rule_id),
            axum::extract::Query(DeleteRuleParams { future: Some("retain".to_string()) }),
            State(Arc::clone(&state)),
            AuthClaims(claims_shared.clone()),
        )
        .await
        .into_response();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);

        Ok(())
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn api_user_rules_own_vs_foreign_matrix() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let state = live_request_state(dir.path(), shared::model::TargetType::Xtream).await?;

        let api_proxy_dto = shared::model::ApiProxyConfigDto {
            user: vec![shared::model::TargetUserDto {
                target: "1".to_string(),
                credentials: vec![
                    shared::model::ProxyUserCredentialsDto {
                        username: "alice".to_string(),
                        password: "pass".to_string(),
                        ui_enabled: true,
                        player_features: Some(shared::model::PlayerFeaturesDto {
                            recording: Some(shared::model::PlayerRecordingLevel::Private),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    shared::model::ProxyUserCredentialsDto {
                        username: "bob".to_string(),
                        password: "pass".to_string(),
                        ui_enabled: true,
                        player_features: Some(shared::model::PlayerFeaturesDto {
                            recording: Some(shared::model::PlayerRecordingLevel::Private),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                ],
            }],
            ..Default::default()
        };
        state.app_config.api_proxy.store(Some(Arc::new(tuliprox_core::model::ApiProxyConfig::from(&api_proxy_dto))));

        let alice_claims = test_api_claims(
            "alice",
            Permission::RecordingRead
                | Permission::RecordingCreate
                | Permission::RecordingManage
                | Permission::RecordingDelete,
        );
        let bob_claims = test_api_claims(
            "bob",
            Permission::RecordingRead
                | Permission::RecordingCreate
                | Permission::RecordingManage
                | Permission::RecordingDelete,
        );
        let admin_claims = edit_claims(Some(UserId::builtin_admin()), true);

        let make_body = || CreateRecordingRuleBody {
            target_id: Some("1".to_string()),
            virtual_id: "42".to_string(),
            input_name: String::new(),
            channel_id: None,
            visibility: Some(RuleVisibility::Private),
            body: RuleBody::WeeklyTimeslot {
                weekday: 1,
                local_start_time: "18:00".to_string(),
                duration_secs: 1800,
                timezone: "UTC".to_string(),
            },
            pre_roll_secs: 0,
            post_roll_secs: 0,
        };

        // Bob creates a rule
        let res = create_recording_rule(State(Arc::clone(&state)), AuthClaims(bob_claims.clone()), Json(make_body()))
            .await
            .into_response();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
        let json: serde_json::Value = serde_json::from_slice(&bytes)?;
        let bob_rule_id = json["id"].as_str().unwrap().to_string();

        // Alice tries to edit Bob's rule -> 403 recording_rule_not_owner
        let edit_body = EditRecordingRuleBody { pre_roll_secs: Some(30), ..Default::default() };
        let res = edit_recording_rule(
            axum::extract::Path(bob_rule_id.clone()),
            State(Arc::clone(&state)),
            AuthClaims(alice_claims.clone()),
            Json(edit_body.clone()),
        )
        .await
        .into_response();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
        let json: serde_json::Value = serde_json::from_slice(&bytes)?;
        assert_eq!(json["error"], "recording_rule_not_owner");

        // Alice tries to delete Bob's rule -> 403 recording_rule_not_owner
        let res = delete_recording_rule(
            axum::extract::Path(bob_rule_id.clone()),
            axum::extract::Query(DeleteRuleParams { future: Some("retain".to_string()) }),
            State(Arc::clone(&state)),
            AuthClaims(alice_claims.clone()),
        )
        .await
        .into_response();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
        let json: serde_json::Value = serde_json::from_slice(&bytes)?;
        assert_eq!(json["error"], "recording_rule_not_owner");

        // Admin can edit Bob's rule
        let res = edit_recording_rule(
            axum::extract::Path(bob_rule_id.clone()),
            State(Arc::clone(&state)),
            AuthClaims(admin_claims.clone()),
            Json(edit_body),
        )
        .await
        .into_response();
        assert_eq!(res.status(), StatusCode::OK);

        // Admin can delete Bob's rule
        let res = delete_recording_rule(
            axum::extract::Path(bob_rule_id),
            axum::extract::Query(DeleteRuleParams { future: Some("retain".to_string()) }),
            State(Arc::clone(&state)),
            AuthClaims(admin_claims),
        )
        .await
        .into_response();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);

        Ok(())
    }

    #[tokio::test]
    async fn api_user_rules_foreign_target_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let state = live_request_state(dir.path(), shared::model::TargetType::Xtream).await?;

        let api_proxy_dto = shared::model::ApiProxyConfigDto {
            user: vec![shared::model::TargetUserDto {
                target: "1".to_string(),
                credentials: vec![shared::model::ProxyUserCredentialsDto {
                    username: "alice".to_string(),
                    password: "pass".to_string(),
                    ui_enabled: true,
                    player_features: Some(shared::model::PlayerFeaturesDto {
                        recording: Some(shared::model::PlayerRecordingLevel::Private),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
            }],
            ..Default::default()
        };
        state.app_config.api_proxy.store(Some(Arc::new(tuliprox_core::model::ApiProxyConfig::from(&api_proxy_dto))));

        let alice_claims = test_api_claims(
            "alice",
            Permission::RecordingRead
                | Permission::RecordingCreate
                | Permission::RecordingManage
                | Permission::RecordingDelete,
        );

        let body = CreateRecordingRuleBody {
            target_id: Some("foreign_target".to_string()),
            virtual_id: "42".to_string(),
            input_name: String::new(),
            channel_id: None,
            visibility: Some(RuleVisibility::Private),
            body: RuleBody::WeeklyTimeslot {
                weekday: 1,
                local_start_time: "18:00".to_string(),
                duration_secs: 1800,
                timezone: "UTC".to_string(),
            },
            pre_roll_secs: 0,
            post_roll_secs: 0,
        };

        let res = create_recording_rule(State(Arc::clone(&state)), AuthClaims(alice_claims), Json(body))
            .await
            .into_response();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
        let json: serde_json::Value = serde_json::from_slice(&bytes)?;
        assert_eq!(json["error"], "recording_invalid_source");

        Ok(())
    }

    #[tokio::test]
    async fn api_user_rules_limit_reached() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let state = live_request_state(dir.path(), shared::model::TargetType::Xtream).await?;

        let api_proxy_dto = shared::model::ApiProxyConfigDto {
            user: vec![shared::model::TargetUserDto {
                target: "1".to_string(),
                credentials: vec![shared::model::ProxyUserCredentialsDto {
                    username: "alice".to_string(),
                    password: "pass".to_string(),
                    ui_enabled: true,
                    player_features: Some(shared::model::PlayerFeaturesDto {
                        recording: Some(shared::model::PlayerRecordingLevel::Private),
                        max_recording_rules: Some(2),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
            }],
            ..Default::default()
        };
        state.app_config.api_proxy.store(Some(Arc::new(tuliprox_core::model::ApiProxyConfig::from(&api_proxy_dto))));

        let alice_claims = test_api_claims(
            "alice",
            Permission::RecordingRead
                | Permission::RecordingCreate
                | Permission::RecordingManage
                | Permission::RecordingDelete,
        );

        let make_body = |weekday: u8| CreateRecordingRuleBody {
            target_id: None,
            virtual_id: "42".to_string(),
            input_name: String::new(),
            channel_id: None,
            visibility: Some(RuleVisibility::Private),
            body: RuleBody::WeeklyTimeslot {
                weekday,
                local_start_time: "18:00".to_string(),
                duration_secs: 1800,
                timezone: "UTC".to_string(),
            },
            pre_roll_secs: 0,
            post_roll_secs: 0,
        };

        // Rule 1 -> OK
        let res =
            create_recording_rule(State(Arc::clone(&state)), AuthClaims(alice_claims.clone()), Json(make_body(1)))
                .await
                .into_response();
        assert_eq!(res.status(), StatusCode::OK);

        // Rule 2 -> OK (limit 2 reached)
        let res =
            create_recording_rule(State(Arc::clone(&state)), AuthClaims(alice_claims.clone()), Json(make_body(2)))
                .await
                .into_response();
        assert_eq!(res.status(), StatusCode::OK);

        // Rule 3 -> 400 recording_rule_limit_exceeded
        let res = create_recording_rule(State(Arc::clone(&state)), AuthClaims(alice_claims), Json(make_body(3)))
            .await
            .into_response();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
        let json: serde_json::Value = serde_json::from_slice(&bytes)?;
        assert_eq!(json["error"], "recording_rule_limit_exceeded");

        Ok(())
    }

    #[tokio::test]
    async fn api_user_rules_shared_visibility_content_filter() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let state = live_request_state(dir.path(), shared::model::TargetType::Xtream).await?;

        let api_proxy_dto = shared::model::ApiProxyConfigDto {
            user: vec![
                shared::model::TargetUserDto {
                    target: "1".to_string(),
                    credentials: vec![
                        shared::model::ProxyUserCredentialsDto {
                            username: "alice".to_string(),
                            password: "pass".to_string(),
                            ui_enabled: true,
                            player_features: Some(shared::model::PlayerFeaturesDto {
                                recording: Some(shared::model::PlayerRecordingLevel::Shared),
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                        shared::model::ProxyUserCredentialsDto {
                            username: "bob".to_string(),
                            password: "pass".to_string(),
                            ui_enabled: true,
                            // Bob allows only "Channel 4K" (channel 43); "Channel HD" (channel 42) is filtered out
                            filter: Some(r#"Title = "Channel 4K""#.to_string()),
                            player_features: Some(shared::model::PlayerFeaturesDto {
                                recording: Some(shared::model::PlayerRecordingLevel::Read),
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                    ],
                },
                shared::model::TargetUserDto {
                    target: "other".to_string(),
                    credentials: vec![shared::model::ProxyUserCredentialsDto {
                        username: "carol".to_string(),
                        password: "pass".to_string(),
                        ui_enabled: true,
                        player_features: Some(shared::model::PlayerFeaturesDto {
                            recording: Some(shared::model::PlayerRecordingLevel::Shared),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }],
                },
            ],
            ..Default::default()
        };
        state.app_config.api_proxy.store(Some(Arc::new(tuliprox_core::model::ApiProxyConfig::from(&api_proxy_dto))));

        let bob_claims = test_api_claims("bob", Permission::RecordingRead);

        let make_rule = |target: &str, vid: &str, vis: RuleVisibility, owner: UserId, id: &str| RecordingRule {
            id: id.to_string(),
            owner_id: owner,
            visibility: vis,
            enabled: true,
            source: RuleSource::new(target, vid, "input-a"),
            channel_id: None,
            body: RuleBody::WeeklyTimeslot {
                weekday: 1,
                local_start_time: "20:00".to_string(),
                duration_secs: 1800,
                timezone: "UTC".to_string(),
            },
            pre_roll_secs: 0,
            post_roll_secs: 0,
            created_at: 0,
            updated_at: 0,
        };

        let repo = recording_rule_repo(&state);
        // Rule 1: Shared on target 1, channel 42 (filtered out for Bob)
        repo.create(make_rule("1", "42", RuleVisibility::Shared, UserId::api("alice"), "rule-shared-42")).await?;
        // Rule 2: Shared on target 1, channel 43 (allowed for Bob)
        repo.create(make_rule("1", "43", RuleVisibility::Shared, UserId::api("alice"), "rule-shared-43")).await?;
        // Rule 3: Private owned by Alice
        repo.create(make_rule("1", "43", RuleVisibility::Private, UserId::api("alice"), "rule-priv-alice")).await?;
        // Rule 4: Shared on target "other"
        repo.create(make_rule("other", "43", RuleVisibility::Shared, UserId::api("carol"), "rule-other-target"))
            .await?;

        // Bob lists rules:
        let res = list_recording_rules(State(Arc::clone(&state)), AuthClaims(bob_claims)).await.into_response();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
        let json: Vec<serde_json::Value> = serde_json::from_slice(&bytes)?;

        let visible_ids: Vec<&str> = json.iter().map(|item| item["id"].as_str().unwrap()).collect();
        // Bob only sees rule-shared-43 (same target and allowed by content filter)
        assert_eq!(visible_ids, vec!["rule-shared-43"]);
        assert_eq!(json[0]["own"], false);
        assert!(json[0].get("owner_id").is_none(), "owner_id must be omitted for other users' shared rules");
        assert!(json[0]["source"].get("input_name").is_none(), "input_name must be omitted");
        assert!(json[0]["source"].get("provider_id").is_none(), "provider_id must be omitted");

        Ok(())
    }

    #[tokio::test]
    async fn api_user_rule_derives_real_input_name_and_ignores_client_value() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = tempfile::tempdir()?;
        let state = live_request_state(dir.path(), shared::model::TargetType::Xtream).await?;

        let api_proxy_dto = shared::model::ApiProxyConfigDto {
            user: vec![shared::model::TargetUserDto {
                target: "1".to_string(),
                credentials: vec![shared::model::ProxyUserCredentialsDto {
                    username: "user_private".to_string(),
                    password: "pass".to_string(),
                    ui_enabled: true,
                    player_features: Some(shared::model::PlayerFeaturesDto {
                        recording: Some(shared::model::PlayerRecordingLevel::Private),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
            }],
            ..Default::default()
        };
        state.app_config.api_proxy.store(Some(Arc::new(tuliprox_core::model::ApiProxyConfig::from(&api_proxy_dto))));

        let claims_private = test_api_claims(
            "user_private",
            Permission::RecordingRead
                | Permission::RecordingCreate
                | Permission::RecordingManage
                | Permission::RecordingDelete,
        );

        // 1. Pass a client forged input name - server must override it with real input name "input-a"
        let res = create_recording_rule(
            State(Arc::clone(&state)),
            AuthClaims(claims_private.clone()),
            Json(CreateRecordingRuleBody {
                target_id: None,
                virtual_id: "42".to_string(),
                input_name: "client-forged-input".to_string(),
                channel_id: None,
                visibility: Some(RuleVisibility::Private),
                body: RuleBody::WeeklyTimeslot {
                    weekday: 3,
                    local_start_time: "20:00".to_string(),
                    duration_secs: 3600,
                    timezone: "UTC".to_string(),
                },
                pre_roll_secs: 0,
                post_roll_secs: 0,
            }),
        )
        .await
        .into_response();

        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
        let resp_json: serde_json::Value = serde_json::from_slice(&bytes)?;
        assert_eq!(resp_json["own"], true);
        assert_eq!(resp_json["owner_id"], "api:user_private");
        assert!(resp_json["source"].get("input_name").is_none(), "input_name must be omitted in user response");
        assert!(resp_json["source"].get("provider_id").is_none(), "provider_id must be omitted in user response");

        let rule_id = resp_json["id"].as_str().expect("rule id");
        let repo = recording_rule_repo(&state);
        let stored_rule = repo.list().await?.into_iter().find(|r| r.id == rule_id).expect("stored rule");
        assert_eq!(stored_rule.source.input_name, "input-a");

        // 2. Pass empty input name - server also derives "input-a"
        let res2 = create_recording_rule(
            State(Arc::clone(&state)),
            AuthClaims(claims_private),
            Json(CreateRecordingRuleBody {
                target_id: Some("1".to_string()),
                virtual_id: "42".to_string(),
                input_name: String::new(),
                channel_id: None,
                visibility: Some(RuleVisibility::Private),
                body: RuleBody::WeeklyTimeslot {
                    weekday: 4,
                    local_start_time: "21:00".to_string(),
                    duration_secs: 1800,
                    timezone: "UTC".to_string(),
                },
                pre_roll_secs: 0,
                post_roll_secs: 0,
            }),
        )
        .await
        .into_response();

        assert_eq!(res2.status(), StatusCode::OK);
        let bytes2 = axum::body::to_bytes(res2.into_body(), usize::MAX).await?;
        let resp_json2: serde_json::Value = serde_json::from_slice(&bytes2)?;
        assert_eq!(resp_json2["own"], true);
        assert!(resp_json2["source"].get("input_name").is_none());

        let rule_id2 = resp_json2["id"].as_str().expect("rule id");
        let stored_rule2 = repo.list().await?.into_iter().find(|r| r.id == rule_id2).expect("stored rule");
        assert_eq!(stored_rule2.source.input_name, "input-a");

        Ok(())
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
            group: None,
            series_name: None,
            extension: Some("ts".to_string()),
            downloadable: true,
            provider_id: None,
            is_group: false,
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

        assert_eq!(parsed.target_id.as_deref(), Some("default"));
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

        let rule = recording_rule_from_create(UserId::from("web:alice"), body, "default".to_string(), 123);

        assert_eq!(rule.source.target_id, "default");
        assert!(matches!(rule.body, RuleBody::NewEpisode { series_id: Some(ref id), .. } if id == "series-1"));
    }

    #[test]
    fn create_recording_source_rejects_numeric_target() {
        let parsed = serde_json::from_value::<RecordingSourceRequest>(json!({
            "target_id": 7,
            "virtual_id": "42",
            "cluster": "Live",
            "input_name": "input-a"
        }));

        assert!(parsed.is_err());
    }

    #[test]
    fn create_recording_source_accepts_string_target() {
        let parsed: RecordingSourceRequest = serde_json::from_value(json!({
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

    /// Method and path pairs the router must answer, and the ones it must
    /// not. `METHOD_NOT_ALLOWED` proves the path exists but the method is
    /// wrong; `NOT_FOUND` proves the path is gone.
    async fn route_status(method: &str, path: &str) -> StatusCode {
        use tower::ServiceExt;
        let router = recording_api_register(axum::Router::new());
        let app_state = crate::api::model::create_test_app_state(tuliprox_core::model::Config::default());
        router
            .with_state(app_state)
            .oneshot(
                axum::http::Request::builder()
                    .method(method)
                    .uri(path)
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response")
            .status()
    }

    #[tokio::test]
    async fn the_command_routes_exist_at_the_documented_paths() {
        // A user addresses their own request; only an administrator
        // addresses the materialization behind it. The two id spaces are
        // deliberately different paths so a library-entry id can never be
        // passed to a command that stops shared physical work.
        for (method, path) in [
            ("POST", "/recording/requests"),
            ("PATCH", "/recording/requests/abc"),
            ("POST", "/recording/requests/abc/cancel"),
            ("DELETE", "/recording/requests/abc"),
            ("POST", "/recording/materializations/abc/pause"),
            ("POST", "/recording/materializations/abc/resume"),
            ("POST", "/recording/materializations/abc/retry"),
            ("DELETE", "/recording/materializations/abc"),
        ] {
            assert_ne!(route_status(method, path).await, StatusCode::NOT_FOUND, "missing route: {method} {path}");
        }
    }

    #[tokio::test]
    async fn there_is_no_rest_route_that_lists_recordings_or_reports_quota() {
        // The snapshot is the only description of recording state. A GET
        // list is what a client polls, and a second description is what
        // lets the two disagree.
        for (method, path) in [("GET", "/recording/tasks"), ("GET", "/recording/quota")] {
            assert_eq!(route_status(method, path).await, StatusCode::NOT_FOUND, "route still present: {method} {path}");
        }
        // The requests path exists, but only as a command endpoint: a GET
        // against it is refused rather than quietly serving a list.
        assert_eq!(route_status("GET", "/recording/requests").await, StatusCode::METHOD_NOT_ALLOWED);
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

    fn test_api_claims(username: &str, perms: impl Into<shared::model::PermissionSet>) -> shared::model::Claims {
        shared::model::Claims {
            username: username.to_string(),
            iss: "tuliprox".to_string(),
            iat: 0,
            exp: 0,
            roles: shared::model::RoleSet::new(),
            permissions: perms.into(),
            pwd_version: 0,
            subject_id: Some(UserId::from(format!("api:{username}"))),
            permission_schema_version: shared::model::CURRENT_PERMISSION_SCHEMA_VERSION,
        }
    }

    #[tokio::test]
    async fn api_user_source_scoping_matrix() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let state = live_request_state(dir.path(), shared::model::TargetType::Xtream).await?;

        let api_proxy_dto = shared::model::ApiProxyConfigDto {
            user: vec![
                shared::model::TargetUserDto {
                    target: "1".to_string(),
                    credentials: vec![
                        shared::model::ProxyUserCredentialsDto {
                            username: "alice".to_string(),
                            password: "pass".to_string(),
                            ui_enabled: true,
                            ..Default::default()
                        },
                        shared::model::ProxyUserCredentialsDto {
                            username: "bob".to_string(),
                            password: "pass".to_string(),
                            ui_enabled: true,
                            filter: Some(r#"Title = "Channel HD""#.to_string()),
                            ..Default::default()
                        },
                    ],
                },
                shared::model::TargetUserDto {
                    target: "other".to_string(),
                    credentials: vec![shared::model::ProxyUserCredentialsDto {
                        username: "carol".to_string(),
                        password: "pass".to_string(),
                        ui_enabled: true,
                        ..Default::default()
                    }],
                },
            ],
            ..Default::default()
        };
        state.app_config.api_proxy.store(Some(Arc::new(tuliprox_core::model::ApiProxyConfig::from(&api_proxy_dto))));

        let alice_claims = test_api_claims("alice", Permission::RecordingCreate | Permission::RecordingRead);
        let bob_claims = test_api_claims("bob", Permission::RecordingCreate | Permission::RecordingRead);
        let carol_claims = test_api_claims("carol", Permission::RecordingCreate | Permission::RecordingRead);

        let start = chrono::Utc::now().timestamp() + 3_600;

        // 1. API user records a channel of another target -> recording_invalid_source
        let foreign_body = serde_json::from_value(json!({
            "source": {"target_id": "1", "virtual_id": "42", "input_name": "input-a", "cluster": "Live"},
            "program_title": "News",
            "program_start": start,
            "program_end": start + 120,
            "visibility": "private",
            "channel_id": "43",
            "channel_name": "Channel HD"
        }))?;
        let foreign_res = create_recording_request(
            State(Arc::clone(&state)),
            AuthClaims(carol_claims),
            HeaderMap::new(),
            Json(foreign_body),
        )
        .await
        .into_response();
        assert_eq!(foreign_res.status(), StatusCode::BAD_REQUEST);
        let foreign_bytes = axum::body::to_bytes(foreign_res.into_body(), usize::MAX).await?;
        let foreign_json: serde_json::Value = serde_json::from_slice(&foreign_bytes)?;
        assert_eq!(foreign_json["error"], "recording_invalid_source");

        // 2. Filtered channel -> recording_invalid_source (Bob's filter allows only "Channel HD", channel 43 is "Channel 4K")
        let filtered_body = serde_json::from_value(json!({
            "source": {"target_id": "1", "virtual_id": "43", "input_name": "input-b", "cluster": "Live"},
            "program_title": "News 4K",
            "program_start": start,
            "program_end": start + 120,
            "visibility": "private",
            "channel_id": "43",
            "channel_name": "Channel 4K"
        }))?;
        let filtered_res = create_recording_request(
            State(Arc::clone(&state)),
            AuthClaims(bob_claims),
            HeaderMap::new(),
            Json(filtered_body),
        )
        .await
        .into_response();
        assert_eq!(filtered_res.status(), StatusCode::BAD_REQUEST);
        let filtered_bytes = axum::body::to_bytes(filtered_res.into_body(), usize::MAX).await?;
        let filtered_json: serde_json::Value = serde_json::from_slice(&filtered_bytes)?;
        assert_eq!(filtered_json["error"], "recording_invalid_source");

        // 3. Own visible channel -> accepted (Alice records channel 42 on target 1)
        let visible_body = serde_json::from_value(json!({
            "source": {"target_id": "1", "virtual_id": "42", "input_name": "input-a", "cluster": "Live"},
            "program_title": "News",
            "program_start": start,
            "program_end": start + 120,
            "visibility": "private",
            "channel_id": "43",
            "channel_name": "Channel HD"
        }))?;
        let visible_res = create_recording_request(
            State(Arc::clone(&state)),
            AuthClaims(alice_claims),
            HeaderMap::new(),
            Json(visible_body),
        )
        .await
        .into_response();
        assert_eq!(visible_res.status(), StatusCode::NO_CONTENT);

        Ok(())
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines, clippy::similar_names)]
    async fn shared_recording_visibility_matrix() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let state = live_request_state(dir.path(), shared::model::TargetType::Xtream).await?;

        let api_proxy_dto = shared::model::ApiProxyConfigDto {
            user: vec![
                shared::model::TargetUserDto {
                    target: "1".to_string(),
                    credentials: vec![
                        shared::model::ProxyUserCredentialsDto {
                            username: "alice".to_string(),
                            password: "pass".to_string(),
                            ui_enabled: true,
                            ..Default::default()
                        },
                        shared::model::ProxyUserCredentialsDto {
                            username: "bob".to_string(),
                            password: "pass".to_string(),
                            ui_enabled: true,
                            // Bob's filter only allows "Channel 4K" (43), so "Channel HD" (42) is hidden from Bob
                            filter: Some(r#"Title = "Channel 4K""#.to_string()),
                            ..Default::default()
                        },
                        shared::model::ProxyUserCredentialsDto {
                            username: "dave".to_string(),
                            password: "pass".to_string(),
                            ui_enabled: true,
                            ..Default::default()
                        },
                    ],
                },
                shared::model::TargetUserDto {
                    target: "other".to_string(),
                    credentials: vec![shared::model::ProxyUserCredentialsDto {
                        username: "carol".to_string(),
                        password: "pass".to_string(),
                        ui_enabled: true,
                        ..Default::default()
                    }],
                },
            ],
            ..Default::default()
        };
        state.app_config.api_proxy.store(Some(Arc::new(tuliprox_core::model::ApiProxyConfig::from(&api_proxy_dto))));

        let alice_claims = test_api_claims(
            "alice",
            Permission::RecordingCreate | Permission::RecordingShare | Permission::RecordingRead,
        );
        let bob_claims = test_api_claims("bob", Permission::RecordingCreate | Permission::RecordingRead);
        let carol_claims = test_api_claims("carol", Permission::RecordingCreate | Permission::RecordingRead);

        let start = chrono::Utc::now().timestamp() + 3_600;

        // 1. User A (no filter) creates shared recording on channel X (42: "Channel HD") of target T ("1")
        let shared_body = serde_json::from_value(json!({
            "source": {"target_id": "1", "virtual_id": "42", "input_name": "input-a", "cluster": "Live"},
            "program_title": "News",
            "program_start": start,
            "program_end": start + 120,
            "visibility": "shared",
            "channel_id": "43",
            "channel_name": "Channel HD"
        }))?;
        let shared_response = create_recording_request(
            State(Arc::clone(&state)),
            AuthClaims(alice_claims.clone()),
            HeaderMap::new(),
            Json(shared_body),
        )
        .await
        .into_response();
        assert_eq!(shared_response.status(), StatusCode::NO_CONTENT);

        let (_, tasks) = state.recording.queue.committed_snapshot().await;
        assert_eq!(tasks.len(), 1);
        let shared_task = tasks[0].clone();

        // Check snapshot for Alice (creator, no filter, same target): sees the task
        let vis = AppRecordingVisibility::new(&state);
        let alice_snapshot = crate::api::model::recording::recording_ws::recording_snapshot(
            &state.recording.queue,
            &alice_claims,
            &state.app_config,
            Some(&vis),
        )
        .await;
        assert_eq!(alice_snapshot.tasks.len(), 1);

        // 2. User B on T with filter hiding X (42) sees neither the task in snapshot nor can access it
        let vis = AppRecordingVisibility::new(&state);
        let bob_snapshot = crate::api::model::recording::recording_ws::recording_snapshot(
            &state.recording.queue,
            &bob_claims,
            &state.app_config,
            Some(&vis),
        )
        .await;
        assert_eq!(bob_snapshot.tasks.len(), 0, "Bob should not see filtered shared recording in snapshot");

        let bob_thumb = crate::api::endpoints::recording_media_api::thumbnail_recording(
            State(Arc::clone(&state)),
            AuthClaims(bob_claims.clone()),
            axum::extract::Path(shared_task.uuid.clone()),
        )
        .await;
        assert_eq!(
            bob_thumb.status(),
            StatusCode::FORBIDDEN,
            "Bob cannot access thumbnail of filtered shared recording"
        );

        // 3. User C on another target ("other") cannot see it
        let vis = AppRecordingVisibility::new(&state);
        let carol_snapshot = crate::api::model::recording::recording_ws::recording_snapshot(
            &state.recording.queue,
            &carol_claims,
            &state.app_config,
            Some(&vis),
        )
        .await;
        assert_eq!(carol_snapshot.tasks.len(), 0, "Carol on another target cannot see shared recording");

        let carol_thumb = crate::api::endpoints::recording_media_api::thumbnail_recording(
            State(Arc::clone(&state)),
            AuthClaims(carol_claims),
            axum::extract::Path(shared_task.uuid.clone()),
        )
        .await;
        assert_eq!(carol_thumb.status(), StatusCode::FORBIDDEN, "Carol on another target cannot access media");

        // 4. Unresolvable vid -> hidden
        let mut unresolvable_task = shared_task.clone();
        unresolvable_task.uuid = "unresolvable-task-1".to_string();
        unresolvable_task.recording.source.virtual_id = "999999".to_string();
        state.recording.queue.scheduled.write().await.push(unresolvable_task);

        let vis = AppRecordingVisibility::new(&state);
        let bob_snapshot_unresolvable = crate::api::model::recording::recording_ws::recording_snapshot(
            &state.recording.queue,
            &bob_claims,
            &state.app_config,
            Some(&vis),
        )
        .await;
        assert_eq!(bob_snapshot_unresolvable.tasks.len(), 0, "Unresolvable vid hidden from filtered user");

        // 5. B's own private recording stays visible
        let bob_private_body = serde_json::from_value(json!({
            "source": {"target_id": "1", "virtual_id": "43", "input_name": "input-b", "cluster": "Live"},
            "program_title": "Bob News 4K",
            "program_start": start + 300,
            "program_end": start + 420,
            "visibility": "private",
            "channel_id": "43",
            "channel_name": "Channel 4K"
        }))?;
        let bob_private_response = create_recording_request(
            State(Arc::clone(&state)),
            AuthClaims(bob_claims.clone()),
            HeaderMap::new(),
            Json(bob_private_body),
        )
        .await
        .into_response();
        assert_eq!(bob_private_response.status(), StatusCode::NO_CONTENT);

        let vis = AppRecordingVisibility::new(&state);
        let bob_snapshot_with_private = crate::api::model::recording::recording_ws::recording_snapshot(
            &state.recording.queue,
            &bob_claims,
            &state.app_config,
            Some(&vis),
        )
        .await;
        assert_eq!(bob_snapshot_with_private.tasks.len(), 1, "Bob's own private recording is visible to Bob");
        assert_eq!(bob_snapshot_with_private.tasks[0].title, "Bob News 4K");

        // 6. One visibility instance must not reuse a same-vid decision across targets.
        let dave_claims = test_api_claims("dave", Permission::RecordingRead);
        let dave_subject = dave_claims.subject_id.clone().ok_or("dave subject")?;
        let mut foreign_meta = shared_task.recording.clone();
        foreign_meta.source.target_id = "other".to_string();
        let vis = AppRecordingVisibility::new(&state);
        assert!(
            check_recording_visibility_with_context(
                &state,
                &shared_task.recording,
                &dave_claims,
                &dave_subject,
                Some(&vis)
            )
            .await,
            "Dave sees the shared recording of his own target"
        );
        assert!(
            !check_recording_visibility_with_context(&state, &foreign_meta, &dave_claims, &dave_subject, Some(&vis))
                .await,
            "a cached same-vid decision must not leak a recording of another target"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_check_recording_visibility_enforces_item_type_on_shared() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let state = live_request_state(dir.path(), shared::model::TargetType::Xtream).await?;

        let api_proxy_dto = shared::model::ApiProxyConfigDto {
            user: vec![shared::model::TargetUserDto {
                target: "1".to_string(),
                credentials: vec![
                    shared::model::ProxyUserCredentialsDto {
                        username: "alice".to_string(),
                        password: "pass".to_string(),
                        ui_enabled: true,
                        ..Default::default()
                    },
                    shared::model::ProxyUserCredentialsDto {
                        username: "dave".to_string(),
                        password: "pass".to_string(),
                        ui_enabled: true,
                        output_clusters: Some(shared::model::ClusterFlags::Vod),
                        ..Default::default()
                    },
                ],
            }],
            ..Default::default()
        };
        state.app_config.api_proxy.store(Some(Arc::new(tuliprox_core::model::ApiProxyConfig::from(&api_proxy_dto))));

        let alice_claims = test_api_claims(
            "alice",
            Permission::RecordingCreate | Permission::RecordingShare | Permission::RecordingRead,
        );
        let dave_claims = test_api_claims("dave", Permission::RecordingCreate | Permission::RecordingRead);

        let start = chrono::Utc::now().timestamp() + 3_600;
        let shared_body = serde_json::from_value(json!({
            "source": {"target_id": "1", "virtual_id": "42", "input_name": "input-a", "cluster": "Live"},
            "program_title": "News",
            "program_start": start,
            "program_end": start + 120,
            "visibility": "shared",
            "channel_id": "42",
            "channel_name": "Channel HD"
        }))?;
        let create_resp = create_recording_request(
            State(Arc::clone(&state)),
            AuthClaims(alice_claims),
            HeaderMap::new(),
            Json(shared_body),
        )
        .await
        .into_response();
        assert_eq!(create_resp.status(), StatusCode::NO_CONTENT);

        let (_, tasks) = state.recording.queue.committed_snapshot().await;
        assert_eq!(tasks.len(), 1);
        let recorded_item = &tasks[0].recording;

        let dave_subject = dave_claims.subject_id.as_ref().unwrap();

        // 1. Dave has output_clusters = [Vod] -> cannot see shared Live recording
        let visible = check_recording_visibility(&state, recorded_item, &dave_claims, dave_subject).await;
        assert!(!visible, "shared Live recording must be hidden from user with only Vod cluster");

        // 2. Dave has output_clusters = [Live] -> can see shared Live recording
        let mut api_proxy_dto2 = api_proxy_dto.clone();
        api_proxy_dto2.user[0].credentials[1].output_clusters = Some(shared::model::ClusterFlags::Live);
        state.app_config.api_proxy.store(Some(Arc::new(tuliprox_core::model::ApiProxyConfig::from(&api_proxy_dto2))));

        let visible = check_recording_visibility(&state, recorded_item, &dave_claims, dave_subject).await;
        assert!(visible, "shared Live recording must be visible to user with Live cluster");

        Ok(())
    }

    #[tokio::test]
    async fn test_edit_recording_rule_source_resolution_behavior() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let state = live_request_state(dir.path(), shared::model::TargetType::Xtream).await?;
        let repo = recording_rule_repo(&state);

        let rule = RecordingRule {
            id: "rule-unresolvable".into(),
            owner_id: UserId::api("alice"),
            visibility: RuleVisibility::Private,
            enabled: true,
            source: RuleSource::new("target-1", "99999", "input-a"),
            channel_id: None,
            body: RuleBody::WeeklyTimeslot {
                weekday: 1,
                local_start_time: "20:00".to_string(),
                duration_secs: 1800,
                timezone: "UTC".to_string(),
            },
            pre_roll_secs: 0,
            post_roll_secs: 0,
            created_at: 100,
            updated_at: 100,
        };
        repo.create(rule).await?;

        let alice_claims = test_api_claims("alice", Permission::RecordingManage);
        let admin_claims = edit_claims(Some(UserId::builtin_admin()), true);

        // 1. API user edits enabled / padding on unresolvable rule: succeeds without re-resolving source
        let patch_enabled =
            EditRecordingRuleBody { enabled: Some(false), pre_roll_secs: Some(120), ..Default::default() };
        let res = edit_recording_rule(
            axum::extract::Path("rule-unresolvable".to_string()),
            State(Arc::clone(&state)),
            AuthClaims(alice_claims.clone()),
            Json(patch_enabled),
        )
        .await
        .into_response();
        assert_eq!(res.status(), StatusCode::OK);

        // 2. Admin edits enabled / padding on unresolvable rule: also succeeds
        let patch_admin = EditRecordingRuleBody { enabled: Some(true), ..Default::default() };
        let res = edit_recording_rule(
            axum::extract::Path("rule-unresolvable".to_string()),
            State(Arc::clone(&state)),
            AuthClaims(admin_claims.clone()),
            Json(patch_admin),
        )
        .await
        .into_response();
        assert_eq!(res.status(), StatusCode::OK);

        // 3. Admin edits channel_id on unresolvable rule: succeeds by keeping stored source
        let patch_ch =
            EditRecordingRuleBody { channel_id: Some("nonexistent_channel".to_string()), ..Default::default() };
        let res = edit_recording_rule(
            axum::extract::Path("rule-unresolvable".to_string()),
            State(Arc::clone(&state)),
            AuthClaims(admin_claims),
            Json(patch_ch.clone()),
        )
        .await
        .into_response();
        assert_eq!(res.status(), StatusCode::OK);

        // 4. API user edits channel_id on unresolvable rule: fails resolution
        let res = edit_recording_rule(
            axum::extract::Path("rule-unresolvable".to_string()),
            State(Arc::clone(&state)),
            AuthClaims(alice_claims),
            Json(patch_ch),
        )
        .await
        .into_response();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        Ok(())
    }

    #[tokio::test]
    async fn test_create_rule_canonicalizes_numeric_target_id_to_name() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let state = live_request_state(dir.path(), shared::model::TargetType::Xtream).await?;

        let api_proxy_dto = shared::model::ApiProxyConfigDto {
            user: vec![shared::model::TargetUserDto {
                target: "1".to_string(),
                credentials: vec![shared::model::ProxyUserCredentialsDto {
                    username: "alice".to_string(),
                    password: "pass".to_string(),
                    ui_enabled: true,
                    player_features: Some(shared::model::PlayerFeaturesDto {
                        recording: Some(shared::model::PlayerRecordingLevel::Private),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
            }],
            ..Default::default()
        };
        state.app_config.api_proxy.store(Some(Arc::new(tuliprox_core::model::ApiProxyConfig::from(&api_proxy_dto))));

        let alice_claims = test_api_claims("alice", Permission::RecordingCreate | Permission::RecordingManage);

        // Pass numeric target id "11" for target named "1" (whose numeric id in live_request_state is 11)
        let body = CreateRecordingRuleBody {
            target_id: Some("11".to_string()),
            virtual_id: "42".to_string(),
            input_name: String::new(),
            body: RuleBody::WeeklyTimeslot {
                weekday: 1,
                local_start_time: "20:00".to_string(),
                duration_secs: 1800,
                timezone: "UTC".to_string(),
            },
            visibility: Some(RuleVisibility::Private),
            channel_id: None,
            pre_roll_secs: 0,
            post_roll_secs: 0,
        };

        let res = create_recording_rule(State(Arc::clone(&state)), AuthClaims(alice_claims), Json(body))
            .await
            .into_response();
        assert_eq!(res.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
        let json: serde_json::Value = serde_json::from_slice(&bytes)?;
        let rule_id = json["id"].as_str().unwrap();

        let repo = recording_rule_repo(&state);
        let stored = repo.list().await?.into_iter().find(|r| r.id == rule_id).unwrap();
        assert_eq!(stored.source.target_id, "1", "target_id must be canonicalized to target NAME");

        Ok(())
    }

    #[tokio::test]
    async fn test_api_user_edit_recording_rule_channel_id_forbidden_by_filter() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = tempfile::tempdir()?;
        let state = live_request_state(dir.path(), shared::model::TargetType::Xtream).await?;

        let api_proxy_dto = shared::model::ApiProxyConfigDto {
            user: vec![shared::model::TargetUserDto {
                target: "1".to_string(),
                credentials: vec![shared::model::ProxyUserCredentialsDto {
                    username: "bob".to_string(),
                    password: "pass".to_string(),
                    ui_enabled: true,
                    // Bob allows only "Channel HD" (VID 42); "Legacy" (VID 45, epg "shared.epg") is filtered out
                    filter: Some(r#"Title = "Channel HD""#.to_string()),
                    player_features: Some(shared::model::PlayerFeaturesDto {
                        recording: Some(shared::model::PlayerRecordingLevel::Private),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
            }],
            ..Default::default()
        };
        state.app_config.api_proxy.store(Some(Arc::new(tuliprox_core::model::ApiProxyConfig::from(&api_proxy_dto))));

        let bob_claims = test_api_claims("bob", Permission::RecordingCreate | Permission::RecordingManage);

        // First verify what create returns when Bob tries to target "shared.epg" (forbidden channel)
        let create_forbidden = CreateRecordingRuleBody {
            target_id: Some("1".to_string()),
            virtual_id: String::new(),
            input_name: String::new(),
            channel_id: Some("shared.epg".to_string()),
            body: RuleBody::WeeklyTimeslot {
                weekday: 1,
                local_start_time: "20:00".to_string(),
                duration_secs: 1800,
                timezone: "UTC".to_string(),
            },
            visibility: Some(RuleVisibility::Private),
            pre_roll_secs: 0,
            post_roll_secs: 0,
        };
        let create_res =
            create_recording_rule(State(Arc::clone(&state)), AuthClaims(bob_claims.clone()), Json(create_forbidden))
                .await
                .into_response();
        assert_eq!(create_res.status(), StatusCode::BAD_REQUEST);
        let create_bytes = axum::body::to_bytes(create_res.into_body(), usize::MAX).await?;
        let create_err: serde_json::Value = serde_json::from_slice(&create_bytes)?;
        assert_eq!(create_err["error"], "recording_invalid_source");

        // Bob has an existing rule pointing to channel 45 ("shared.epg" -> "Legacy", VID 45)
        let rule = RecordingRule {
            id: "bob-rule-1".into(),
            owner_id: UserId::api("bob"),
            visibility: RuleVisibility::Private,
            enabled: true,
            source: RuleSource::new("1", "45", "input-a"),
            channel_id: Some("shared.epg".to_string()),
            body: RuleBody::WeeklyTimeslot {
                weekday: 1,
                local_start_time: "20:00".to_string(),
                duration_secs: 1800,
                timezone: "UTC".to_string(),
            },
            pre_roll_secs: 0,
            post_roll_secs: 0,
            created_at: 100,
            updated_at: 100,
        };
        let repo = recording_rule_repo(&state);
        repo.create(rule).await?;

        // Bob edits his rule's channel_id to "shared.epg" (forbidden by his filter)
        let patch = EditRecordingRuleBody { channel_id: Some("shared.epg".to_string()), ..Default::default() };
        let edit_res = edit_recording_rule(
            axum::extract::Path("bob-rule-1".to_string()),
            State(Arc::clone(&state)),
            AuthClaims(bob_claims),
            Json(patch),
        )
        .await
        .into_response();

        // Must be refused with the same error create returns
        assert_eq!(edit_res.status(), StatusCode::BAD_REQUEST);
        let edit_bytes = axum::body::to_bytes(edit_res.into_body(), usize::MAX).await?;
        let edit_err: serde_json::Value = serde_json::from_slice(&edit_bytes)?;
        assert_eq!(edit_err, create_err);

        Ok(())
    }
}
