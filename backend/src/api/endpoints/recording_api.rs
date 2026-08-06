//! Recording REST routes.

use std::sync::Arc;

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json, Router};
use serde::{Deserialize, Serialize};

use crate::api::endpoints::recording_media_api::AuthClaims;
use crate::api::model::recording_service::{
    CreateRecordingInput, EditRecordingPatch, RecordingService, RecordingSourceInput, ServiceError,
};
use crate::api::model::recording_rule_service::{DeleteFuture, RuleServiceError};
use crate::api::model::{recording_quota, recording_ws, FileDownload};
use crate::api::model::AppState;
use crate::repository::recording_rule_repository::RecordingRuleRepository;
use shared::model::{
    recording::{RecordingProvenance, RecordingVisibility},
    recording_rule::{RecordingRule, RuleBody, RuleSource, RuleVisibility},
    FileDownloadDto, Permission, UserId, XtreamCluster, ROLE_ADMIN,
};
use axum::routing::{get, patch, post};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: &'static str,
}

fn error_response(status: StatusCode, error: &'static str) -> axum::response::Response {
    (status, Json(ErrorResponse { error })).into_response()
}

fn service_error_response(err: &ServiceError) -> axum::response::Response {
    let status = match err {
        ServiceError::UnknownOwner => StatusCode::UNAUTHORIZED,
        ServiceError::InvalidSource
        | ServiceError::InvalidInterval
        | ServiceError::InvalidState
        | ServiceError::QuotaExceeded => StatusCode::BAD_REQUEST,
        ServiceError::Forbidden | ServiceError::SharedCreationNotAdministrator => StatusCode::FORBIDDEN,
        ServiceError::UnknownRecording => StatusCode::NOT_FOUND,
        ServiceError::PersistenceFailed | ServiceError::IoError(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    error_response(status, err.code())
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
    let (revision, mut tasks) = recording_ws::recording_snapshot(&app_state.downloads, &claims);
    if let Some(owner) = params.owner.as_deref() {
        tasks.retain(|task| {
            task.recording
                .as_ref()
                .and_then(|recording| recording.owner_id.as_ref())
                .is_some_and(|id| id.0 == owner)
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
    let mut source = body.source;
    resolve_epg_recording_source_if_needed(
        &app_state,
        &source.target_id,
        &mut source.virtual_id,
        &mut source.input_name,
        source.cluster,
    )
    .await;
    let service = RecordingService::new(app_state.downloads.clone(), app_state.app_config.clone());
    let input = CreateRecordingInput {
        source: RecordingSourceInput {
            target_id: source.target_id,
            virtual_id: source.virtual_id,
            cluster: source.cluster,
            input_name: source.input_name,
        },
        program_title: body.program_title,
        program_start: body.program_start,
        program_end: body.program_end,
        pre_roll_secs: body.pre_roll_secs,
        post_roll_secs: body.post_roll_secs,
        visibility: body.visibility,
        channel_id: body.channel_id,
        channel_name: body.channel_name,
        provenance: RecordingProvenance::default(),
    };
    match service.create_recording(&claims, &input).await {
        Ok(view) => Json(CreateRecordingTaskResponse {
            id: view.uuid,
            title: view.filename_preview,
            recording: None,
        })
        .into_response(),
        Err(err) => service_error_response(&err),
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateRecordingTaskBody {
    pub source: CreateRecordingSourceBody,
    pub program_title: String,
    pub program_start: i64,
    pub program_end: i64,
    pub pre_roll_secs: u64,
    pub post_roll_secs: u64,
    pub visibility: RecordingVisibility,
    #[serde(default)]
    pub channel_id: Option<String>,
    #[serde(default)]
    pub channel_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateRecordingSourceBody {
    pub target_id: String,
    pub virtual_id: String,
    pub cluster: XtreamCluster,
    pub input_name: String,
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
        Ok(_view) => StatusCode::NO_CONTENT.into_response(),
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
pub async fn cancel_recording_task(
    axum::extract::Path(id): axum::extract::Path<String>,
    State(app_state): State<Arc<AppState>>,
    AuthClaims(claims): AuthClaims,
) -> impl IntoResponse {
    let service = RecordingService::new(app_state.downloads.clone(), app_state.app_config.clone());
    match service.cancel_recording(&claims, &id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
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
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => service_error_response(&err),
    }
}

/// POST /api/v1/recording/conflicts/preview
pub async fn preview_recording_conflicts(
    AuthClaims(_claims): AuthClaims,
    axum::extract::State(_): axum::extract::State<Arc<AppState>>,
    axum::extract::Json(body): axum::extract::Json<PreviewConflictsBody>,
) -> Json<PreviewConflictsResponse> {
    let candidate = crate::api::model::recording_conflict::DemandPoint {
        task_id: body.candidate.task_id,
        padded_start: body.candidate.padded_start,
        padded_end: body.candidate.padded_end,
        priority: body.candidate.priority,
    };
    let others: Vec<crate::api::model::recording_conflict::DemandPoint> = body
        .others
        .into_iter()
        .map(|o| crate::api::model::recording_conflict::DemandPoint {
            task_id: o.task_id,
            padded_start: o.padded_start,
            padded_end: o.padded_end,
            priority: o.priority,
        })
        .collect();
    let capacity = crate::api::model::recording_conflict::EffectiveCapacity {
        background_slots: body.capacity.background_slots,
        reserved_interactive_slots: body.capacity.reserved_interactive_slots,
    };
    let preview = crate::api::model::recording_conflict::preview_conflict(
        &candidate,
        &others,
        capacity,
        body.provider_scope,
    );
    Json(PreviewConflictsResponse {
        severity: preview.severity.as_wire().to_string(),
        provider_scope: preview.provider_scope,
        overlap_segments: preview
            .overlap_segments
            .into_iter()
            .map(|s| OverlapSegmentDto { start: s.start, end: s.end, peak_demand: s.peak_demand })
            .collect(),
    })
}

#[derive(Debug, Clone, Deserialize)]
pub struct PreviewConflictsBody {
    pub candidate: PreviewCandidateDto,
    #[serde(default)]
    pub others: Vec<PreviewOtherDto>,
    pub capacity: PreviewCapacityDto,
    #[serde(default)]
    pub provider_scope: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PreviewCandidateDto {
    /// Opaque task id used for log correlation; never surfaced in the
    /// response. Empty string is allowed for the candidate (the
    /// preview has not yet produced a task id).
    #[serde(default)]
    pub task_id: String,
    pub padded_start: i64,
    pub padded_end: i64,
    pub priority: i32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PreviewOtherDto {
    /// Opaque task id used for log correlation; never surfaced in the
    /// response. The privacy contract in `recording_conflict.rs`
    /// guarantees this id is dropped before the response.
    pub task_id: String,
    pub padded_start: i64,
    pub padded_end: i64,
    pub priority: i32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PreviewCapacityDto {
    pub background_slots: u32,
    pub reserved_interactive_slots: u32,
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
    let limits = quota_limits_from_config(config
        .video
        .as_ref()
        .and_then(|v| v.download.as_ref())
        .and_then(|d| d.recording.as_ref())
        .and_then(|r| r.quota.as_ref()));
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

fn rule_error_response(err: &crate::api::model::recording_rule_service::RuleServiceError) -> axum::response::Response {
    use crate::api::model::recording_rule_service::RuleServiceError;
    let status = match err {
        RuleServiceError::Forbidden
        | RuleServiceError::SharedManagementNotAdministrator
        | RuleServiceError::NotOwner => StatusCode::FORBIDDEN,
        RuleServiceError::InvalidRule | RuleServiceError::InvalidFuture => StatusCode::BAD_REQUEST,
        RuleServiceError::UnknownRule => StatusCode::NOT_FOUND,
        RuleServiceError::PersistenceFailed | RuleServiceError::PartialOperation { .. } => StatusCode::INTERNAL_SERVER_ERROR,
    };
    error_response(status, err.code())
}

fn recording_rule_repo(app_state: &AppState) -> RecordingRuleRepository {
    RecordingRuleRepository::new(&app_state.app_config.config.load().storage_dir)
}

fn can_write_rules(claims: &shared::model::Claims) -> bool {
    claims.permissions.contains(Permission::RecordingWrite)
}

fn is_admin(claims: &shared::model::Claims) -> bool {
    claims.roles.iter().any(|role| role == ROLE_ADMIN)
}

fn rule_timezone(app_state: &AppState) -> String {
    app_state
        .app_config
        .config
        .load()
        .video
        .as_ref()
        .and_then(|v| v.download.as_ref())
        .and_then(|d| d.recording.as_ref())
        .map_or_else(|| "UTC".to_string(), |r| r.timezone.to_string())
}

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

async fn resolve_epg_recording_source_if_needed(
    app_state: &AppState,
    target_id: &str,
    virtual_id: &mut String,
    input_name: &mut String,
    cluster: XtreamCluster,
) {
    if cluster != XtreamCluster::Live || virtual_id.parse::<u32>().is_ok() {
        return;
    }
    let Ok(target) = target_id.parse::<u16>() else {
        return;
    };
    if let Some(resolved) =
        crate::api::endpoints::v1_api_playlist::resolve_target_live_recording_source_by_epg_channel(
            &app_state.app_config,
            target,
            virtual_id,
        )
        .await
    {
        *virtual_id = resolved.virtual_id.to_string();
        if input_name.trim().is_empty() {
            *input_name = resolved.input_name;
        }
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
    let revision = app_state.downloads.revision.load(std::sync::atomic::Ordering::SeqCst);
    Json(
        rules
            .into_iter()
            .filter(|rule| rule.visibility == RuleVisibility::Shared || admin || &rule.owner_id == subject_id)
            .map(|rule| RecordingRuleResponse { id: rule.id, revision })
            .collect::<Vec<_>>(),
    )
    .into_response()
}

#[derive(Debug, Clone, Serialize)]
pub struct RecordingRuleResponse {
    pub id: String,
    pub revision: u64,
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
    resolve_epg_recording_source_if_needed(
        &app_state,
        &body.target_id,
        &mut body.virtual_id,
        &mut body.input_name,
        XtreamCluster::Live,
    )
    .await;
    let now = chrono::Utc::now().timestamp();
    let rule = RecordingRule {
        id: format!("rule-{}-{}", now, shared::utils::generate_random_string(8)),
        owner_id,
        visibility: body.visibility.unwrap_or_default(),
        enabled: true,
        source: RuleSource::new(body.target_id, body.virtual_id, body.input_name),
        channel_id: body.channel_id,
        body: RuleBody::WeeklyTimeslot {
            weekday: body.weekday,
            local_start_time: body.start_time,
            duration_secs: body.duration_secs,
            timezone: body.timezone.unwrap_or_else(|| rule_timezone(&app_state)),
        },
        pre_roll_secs: body.pre_roll_secs,
        post_roll_secs: body.post_roll_secs,
        created_at: now,
        updated_at: now,
    };
    if let Err(err) = crate::api::model::recording_rule_service::validate_rule(&rule)
        .and_then(|()| {
            crate::api::model::recording_rule_service::authorize_rule_action(
                can_write_rules(&claims),
                is_admin(&claims),
                &rule.owner_id,
                &rule,
            )
        })
    {
        return rule_error_response(&err);
    }
    match recording_rule_repo(&app_state).create(rule).await {
        Ok(rule) => Json(RecordingRuleResponse {
            id: rule.id,
            revision: app_state.downloads.revision.load(std::sync::atomic::Ordering::SeqCst),
        })
        .into_response(),
        Err(_) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "recording_persistence_failed"),
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateRecordingRuleBody {
    pub target_id: String,
    pub virtual_id: String,
    pub input_name: String,
    pub weekday: u8,
    pub start_time: String,
    pub duration_secs: u64,
    #[serde(default)]
    pub timezone: Option<String>,
    #[serde(default)]
    pub visibility: Option<RuleVisibility>,
    #[serde(default)]
    pub channel_id: Option<String>,
    #[serde(default)]
    pub pre_roll_secs: u64,
    #[serde(default)]
    pub post_roll_secs: u64,
}

/// PATCH /api/v1/recording/rules/{id}
pub async fn edit_recording_rule(
    axum::extract::Path(id): axum::extract::Path<String>,
    State(app_state): State<Arc<AppState>>,
    AuthClaims(claims): AuthClaims,
    Json(body): Json<EditRecordingRuleBody>,
) -> impl IntoResponse {
    let repo = recording_rule_repo(&app_state);
    let mut rule = match repo.load().await {
        Ok(file) => match file.rules.into_iter().find(|rule| rule.id == id) {
            Some(rule) => rule,
            None => return rule_error_response(&crate::api::model::recording_rule_service::RuleServiceError::UnknownRule),
        },
        Err(_) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, "recording_persistence_failed"),
    };
    if let Err(err) = crate::api::model::recording_rule_service::authorize_rule_action(
        can_write_rules(&claims),
        is_admin(&claims),
        claims.subject_id.as_ref().unwrap_or(&rule.owner_id),
        &rule,
    ) {
        return rule_error_response(&err);
    }
    if let Some(channel_id) = body.channel_id {
        rule.channel_id = Some(channel_id);
    }
    if let Some(pre_roll_secs) = body.pre_roll_secs {
        rule.pre_roll_secs = pre_roll_secs;
    }
    if let Some(post_roll_secs) = body.post_roll_secs {
        rule.post_roll_secs = post_roll_secs;
    }
    if let RuleBody::WeeklyTimeslot { weekday, local_start_time, duration_secs, timezone } = &mut rule.body {
        if let Some(value) = body.weekday {
            *weekday = value;
        }
        if let Some(value) = body.start_time {
            *local_start_time = value;
        }
        if let Some(value) = body.duration_secs {
            *duration_secs = value;
        }
        if let Some(value) = body.timezone {
            *timezone = value;
        }
    }
    rule.updated_at = chrono::Utc::now().timestamp();
    if let Err(err) = crate::api::model::recording_rule_service::validate_rule(&rule) {
        return rule_error_response(&err);
    }
    match repo.update(rule).await {
        Ok(Some(rule)) => Json(RecordingRuleResponse {
            id: rule.id,
            revision: app_state.downloads.revision.load(std::sync::atomic::Ordering::SeqCst),
        })
        .into_response(),
        Ok(None) => rule_error_response(&crate::api::model::recording_rule_service::RuleServiceError::UnknownRule),
        Err(_) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "recording_persistence_failed"),
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct EditRecordingRuleBody {
    #[serde(default)]
    pub weekday: Option<u8>,
    #[serde(default)]
    pub start_time: Option<String>,
    #[serde(default)]
    pub duration_secs: Option<u64>,
    #[serde(default)]
    pub timezone: Option<String>,
    #[serde(default)]
    pub channel_id: Option<String>,
    #[serde(default)]
    pub pre_roll_secs: Option<u64>,
    #[serde(default)]
    pub post_roll_secs: Option<u64>,
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
            None => return rule_error_response(&crate::api::model::recording_rule_service::RuleServiceError::UnknownRule),
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
    if future == DeleteFuture::Cancel {
        let service = RecordingService::new(app_state.downloads.clone(), app_state.app_config.clone());
        if service
            .cancel_future_rule_recordings(&claims, &id, chrono::Utc::now().timestamp())
            .await
            .is_err()
        {
            return rule_error_response(&RuleServiceError::PartialOperation {
                primary: "rule_retained".to_string(),
                secondary: "future_cancel_failed".to_string(),
            });
        }
    }
    match repo.delete(&id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => rule_error_response(&RuleServiceError::UnknownRule),
        Err(_) if future == DeleteFuture::Cancel => rule_error_response(&RuleServiceError::PartialOperation {
            primary: "future_cancelled".to_string(),
            secondary: "rule_delete_failed".to_string(),
        }),
        Err(_) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "recording_persistence_failed"),
    }
}

/// Build the recording router for `/api/v1/recording`.
///
/// The router is a thin wrapper. The state type is `Arc<AppState>`
/// (which matches the rest of the v1 router tree). Production wiring
/// in `v1_api::router_v1` will call
/// `recording_api::router().with_state(Arc::clone(&app_state))`.
pub fn recording_api_register(router: Router<Arc<AppState>>) -> axum::Router<Arc<AppState>> {
    let recording_routes = Router::new()
        .route("/tasks", get(list_recording_tasks).post(create_recording_task))
        .route(
            "/tasks/{id}",
            patch(edit_recording_task).delete(delete_recording_task),
        )
        .route("/tasks/{id}/cancel", post(cancel_recording_task))
        .route("/conflicts/preview", post(preview_recording_conflicts))
        .route("/quota", get(get_recording_quota))
        .route("/rules", get(list_recording_rules).post(create_recording_rule))
        .route(
            "/rules/{id}",
            patch(edit_recording_rule).delete(delete_recording_rule),
        );

    router.nest("/recording", recording_routes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
            serde_json::from_value(json!({"owner": "web:alice", "visibility": "private"}))
                .expect("parse full params");
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
        let parsed: DeleteRuleParams =
            serde_json::from_value(json!({"future": "retain"})).expect("parse retain");
        assert_eq!(parsed.future.as_deref(), Some("retain"));
    }

    #[test]
    fn delete_rule_params_accepts_future_cancel() {
        let parsed: DeleteRuleParams =
            serde_json::from_value(json!({"future": "cancel"})).expect("parse cancel");
        assert_eq!(parsed.future.as_deref(), Some("cancel"));
    }
}
