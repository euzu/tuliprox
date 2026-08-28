//! Frontend recording service.
//!
//! Typed HTTP client over the recording REST routes registered by
//! `backend::api::endpoints::recording_api`. Submits source identifiers
//! rather than URLs, surfaces stable error codes for localization, and
//! signals when a token refresh is required. All recording actions are
//! gated on `recording` permissions, never on `download` permissions.
//!
//! The Playlist Explorer / EPG recording form uses
//! `RecordingService::create_task` to submit the form's
//! `CreateRecordingTaskRequest`. The rest of the API surface is
//! exposed for edit, quota, conflict and rule views.

use crate::{
    error::Error,
    services::{get_base_href, request_delete, request_get, request_patch, request_post, Encoding},
};
use serde::{Deserialize, Serialize};
use shared::{
    model::{
        recording_rule::{RuleBody, RuleVisibility},
        TaskKindDto, TaskPriorityDto, TransferStatusDto, TransferTaskDto, XtreamCluster,
    },
    utils::concat_path_leading_slash,
};

/// Source identifiers (server-resolved) for creating a recording.
/// These come from the configured target/input combination, never a
/// free-form URL.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RecordingSourceInput {
    pub target_id: String,
    pub virtual_id: String,
    pub cluster: XtreamCluster,
    pub input_name: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct CreateRecordingTaskRequest {
    pub source: RecordingSourceInput,
    pub program_title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub program_start: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub program_end: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_roll_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post_roll_secs: Option<u64>,
    pub visibility: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epg: Option<shared::model::recording::EpgEpisodeMetadata>,
}

#[derive(Clone, Debug, Serialize)]
pub struct EditRecordingTaskRequest {
    pub program_start: Option<i64>,
    pub program_end: Option<i64>,
    pub pre_roll_secs: Option<u64>,
    pub post_roll_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub program_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_name: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RecordingTaskId {
    pub id: String,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct RecordingTaskResponse {
    pub id: String,
    pub title: String,
    /// `create_recording_task` returns only `{id, title, recording}`; the
    /// `list_tasks` snapshot returns the full `TransferTaskDto` shape.
    /// `#[serde(default)]` lets both responses deserialize into one type.
    #[serde(default = "default_kind")]
    pub kind: TaskKindDto,
    #[serde(default = "default_priority")]
    pub priority: TaskPriorityDto,
    #[serde(default = "default_status")]
    pub status: TransferStatusDto,
    #[serde(default)]
    pub retry_attempts: u8,
    #[serde(default)]
    pub downloaded_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_retry_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scheduled_start_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub recording: Option<shared::model::recording::RecordingTaskDto>,
}

fn default_priority() -> TaskPriorityDto { TaskPriorityDto::Normal }
fn default_status() -> TransferStatusDto { TransferStatusDto::Scheduled }
fn default_kind() -> TaskKindDto { TaskKindDto::Recording }

impl From<TransferTaskDto> for RecordingTaskResponse {
    fn from(value: TransferTaskDto) -> Self {
        Self {
            id: value.id,
            title: value.title,
            kind: value.kind,
            priority: value.priority,
            status: value.status,
            retry_attempts: value.retry_attempts,
            downloaded_bytes: value.downloaded_bytes,
            total_bytes: value.total_bytes,
            next_retry_at: value.next_retry_at,
            scheduled_start_at: value.scheduled_start_at,
            duration_secs: value.duration_secs,
            error: value.error,
            recording: value.recording,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct RecordingSnapshot {
    pub revision: u64,
    pub tasks: Vec<RecordingTaskResponse>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct OverlapSegmentDto {
    pub start: i64,
    pub end: i64,
    pub peak_demand: u32,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictSeverity {
    NoKnownConflict,
    PossibleCapacityWait,
    LikelyMissedWindow,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct RecordingConflictPreview {
    pub severity: ConflictSeverity,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_scope: Option<String>,
    #[serde(default)]
    pub overlap_segments: Vec<OverlapSegmentDto>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PreviewConflictsRequest {
    /// Server-owned source identifiers. The backend resolves them,
    /// enumerates the committed queue state, and computes capacity
    /// from config; the frontend never submits another recording's
    /// padded interval, capacity, or provider identifier.
    pub source: PreviewSourceDto,
    pub candidate: PreviewCandidateDto,
}

#[derive(Clone, Debug, Serialize)]
pub struct PreviewSourceDto {
    pub target_name: String,
    pub virtual_id: String,
    pub input_name: String,
}

#[derive(Clone, Debug, Serialize)]
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

#[derive(Clone, Debug, Deserialize)]
pub struct RecordingQuota {
    pub private_used_bytes: u64,
    pub private_limit_bytes: Option<u64>,
    pub shared_used_bytes: u64,
    pub shared_limit_bytes: Option<u64>,
    pub revision: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct CreateRecordingRuleRequest {
    pub target_id: String,
    pub virtual_id: String,
    pub input_name: String,
    pub body: RuleBody,
    pub channel_id: Option<String>,
    pub pre_roll_secs: u64,
    pub post_roll_secs: u64,
    pub visibility: RuleVisibility,
}

#[derive(Clone, Debug, Serialize)]
pub struct EditRecordingRuleRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<RuleBody>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<String>,
    #[serde(default, skip_serializing_if = "shared::defaults::is_false")]
    pub clear_channel_id: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_roll_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post_roll_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visibility: Option<RuleVisibility>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct RecordingRuleResponse {
    pub revision: u64,
    #[serde(flatten)]
    pub rule: RecordingRuleSnapshot,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct RecordingRuleSnapshot {
    pub id: String,
    pub owner_id: String,
    pub visibility: shared::model::recording_rule::RuleVisibility,
    pub enabled: bool,
    pub source: shared::model::recording_rule::RuleSource,
    pub channel_id: Option<String>,
    pub body: shared::model::recording_rule::RuleBody,
    pub pre_roll_secs: u64,
    pub post_roll_secs: u64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct DeleteRuleParams {
    pub future: String,
}

/// Stable, frontend-side error mapping. The codes are the source of
/// truth; the i18n layer wires them to display strings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordingError {
    /// Token missing `subject_id` or `permission_schema_version`.
    /// Frontend must trigger a token refresh.
    TokenRefreshRequired,
    /// The supplied `RecordingSource` does not match a configured
    /// target/input combination.
    InvalidSource,
    /// Caller asked for shared creation but is not an administrator.
    SharedCreationNotAdministrator,
    /// `recording_forbidden` — the principal lacks the required
    /// permission for the requested action.
    Forbidden,
    /// Path / kind validation failed (e.g., a partial path on a
    /// Completed recording, a foreign `relative_path`, or a missing
    /// owner).
    InvalidPath,
    /// The recording is in an ineligible state for the requested
    /// action (e.g., edit on `Deleting`).
    InvalidState,
    /// `recording_invalid_interval` — `program_end - program_start`
    /// overflows or is non-positive.
    InvalidInterval,
    /// `recording_unknown` — uuid not in the queue.
    UnknownRecording,
    /// `recording_duplicate` — the queue already contains a matching
    /// task.
    Duplicate,
    /// `recording_path_reservation_failed` — no relative path was free.
    PathReservationFailed,
    /// `recording_quota_exceeded` — owner or shared quota would be
    /// exceeded.
    QuotaExceeded,
    /// `recording_padding_limit_exceeded` — pre-roll or post-roll is
    /// above the configured maximum. Distinct from `InvalidInterval`:
    /// the programme window is fine, only the padding is not.
    PaddingLimitExceeded,
    /// `recording_provenance_immutable` — the patch would clear the
    /// rule provenance of a rule-materialized recording.
    ProvenanceImmutable,
    /// `recording_not_terminal` — deletion was attempted on a
    /// recording that has not finished yet.
    NotTerminal,
    /// `recording_disabled` — the DVR cannot run on this server:
    /// either the `video.download` block is missing from the
    /// configuration, or `video.download.recording.enabled` is
    /// `false`.
    Disabled,
    /// Catch-all for unrecognised codes so the frontend does not
    /// panic on a backend change.
    Other(String),
    /// Network / transport failure (no response, parse error).
    Network(String),
}

impl RecordingError {
    /// Map a backend error-code string (or transport error) to the
    /// frontend enum. Unknown codes fall through to `Other`.
    pub fn from_code(code: &str) -> Self {
        match code {
            "recording_unknown_owner" | "recording_invalid_template" | "recording_token_refresh_required" => {
                Self::TokenRefreshRequired
            }
            "recording_invalid_source" => Self::InvalidSource,
            "recording_invalid_path" => Self::InvalidPath,
            "recording_shared_not_administrator" => Self::SharedCreationNotAdministrator,
            "recording_forbidden" | "recording_rule_forbidden" | "recording_rule_not_owner" => Self::Forbidden,
            "recording_invalid_state" => Self::InvalidState,
            "recording_invalid_interval" => Self::InvalidInterval,
            "recording_invalid_padding" | "recording_padding_limit_exceeded" => Self::PaddingLimitExceeded,
            "recording_provenance_immutable" => Self::ProvenanceImmutable,
            "recording_not_terminal" => Self::NotTerminal,
            "recording_disabled" => Self::Disabled,
            "recording_unknown" | "recording_rule_unknown" => Self::UnknownRecording,
            "recording_duplicate" => Self::Duplicate,
            "recording_path_reservation_failed" => Self::PathReservationFailed,
            "recording_quota_exceeded" => Self::QuotaExceeded,
            "recording_io_error" | "recording_persistence_failed" => Self::Other(code.to_string()),
            "recording_rule_invalid" | "recording_rule_invalid_future" | "recording_rule_partial_operation" => {
                Self::Other(code.to_string())
            }
            other => Self::Other(other.to_string()),
        }
    }

    /// Stable wire code. Used by i18n keys.
    pub fn code(&self) -> &'static str {
        match self {
            Self::TokenRefreshRequired => "recording_token_refresh_required",
            Self::InvalidSource => "recording_invalid_source",
            Self::SharedCreationNotAdministrator => "recording_shared_not_administrator",
            Self::Forbidden => "recording_forbidden",
            Self::InvalidPath => "recording_invalid_path",
            Self::InvalidState => "recording_invalid_state",
            Self::InvalidInterval => "recording_invalid_interval",
            Self::UnknownRecording => "recording_unknown",
            Self::Duplicate => "recording_duplicate",
            Self::PathReservationFailed => "recording_path_reservation_failed",
            Self::QuotaExceeded => "recording_quota_exceeded",
            Self::PaddingLimitExceeded => "recording_padding_limit_exceeded",
            Self::ProvenanceImmutable => "recording_provenance_immutable",
            Self::NotTerminal => "recording_not_terminal",
            Self::Disabled => "recording_disabled",
            Self::Other(_) => "recording_other",
            Self::Network(_) => "recording_network_error",
        }
    }

    /// The i18n key for a user-facing message.
    ///
    /// Error codes used to reach the user as raw strings like
    /// `recording_invalid_state`, or as hand-rolled English in a
    /// `format!`. Every variant now resolves to a key under
    /// `MESSAGES.RECORDING.ERROR`, so the message is translated and a
    /// new backend code degrades to the generic `OTHER` entry rather
    /// than leaking a wire code into the UI.
    pub fn i18n_key(&self) -> &'static str {
        match self {
            Self::TokenRefreshRequired => "MESSAGES.RECORDING.ERROR.TOKEN_REFRESH_REQUIRED",
            Self::InvalidSource => "MESSAGES.RECORDING.ERROR.INVALID_SOURCE",
            Self::SharedCreationNotAdministrator => "MESSAGES.RECORDING.ERROR.SHARED_NOT_ADMINISTRATOR",
            Self::Forbidden => "MESSAGES.RECORDING.ERROR.FORBIDDEN",
            Self::InvalidPath => "MESSAGES.RECORDING.ERROR.INVALID_PATH",
            Self::InvalidState => "MESSAGES.RECORDING.ERROR.INVALID_STATE",
            Self::InvalidInterval => "MESSAGES.RECORDING.ERROR.INVALID_INTERVAL",
            Self::PaddingLimitExceeded => "MESSAGES.RECORDING.ERROR.PADDING_LIMIT_EXCEEDED",
            Self::ProvenanceImmutable => "MESSAGES.RECORDING.ERROR.PROVENANCE_IMMUTABLE",
            Self::NotTerminal => "MESSAGES.RECORDING.ERROR.NOT_TERMINAL",
            Self::Disabled => "MESSAGES.RECORDING.ERROR.DISABLED",
            Self::UnknownRecording => "MESSAGES.RECORDING.ERROR.UNKNOWN",
            Self::Duplicate => "MESSAGES.RECORDING.ERROR.DUPLICATE",
            Self::PathReservationFailed => "MESSAGES.RECORDING.ERROR.PATH_RESERVATION_FAILED",
            Self::QuotaExceeded => "MESSAGES.RECORDING.ERROR.QUOTA_EXCEEDED",
            Self::Network(_) => "MESSAGES.RECORDING.ERROR.NETWORK",
            Self::Other(code) => match code.as_str() {
                "recording_io_error" => "MESSAGES.RECORDING.ERROR.IO_ERROR",
                "recording_persistence_failed" => "MESSAGES.RECORDING.ERROR.PERSISTENCE_FAILED",
                _ => "MESSAGES.RECORDING.ERROR.OTHER",
            },
        }
    }
}

impl std::fmt::Display for RecordingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Other(c) | Self::Network(c) => f.write_str(c),
            other => f.write_str(other.code()),
        }
    }
}

impl std::error::Error for RecordingError {}

/// Frontend recording service. Holds the API base paths; all methods
/// are async and return a `Result<T, RecordingError>` where the error
/// is the typed `RecordingError` (network failures and backend
/// error-code responses are both normalised).
#[derive(Clone)]
pub struct RecordingService {
    base_path: String,
}

impl RecordingService {
    /// Construct with the default `/api/v1/recording` base path.
    pub fn new() -> Self {
        let base = get_base_href();
        Self { base_path: concat_path_leading_slash(&base, "api/v1/recording") }
    }

    /// Override the base path. Tests use this to point at a mock
    /// server; production uses the default constructed by `new`.
    pub fn with_base_path(base_path: String) -> Self { Self { base_path } }

    fn tasks_path(&self) -> String { concat_path_leading_slash(&self.base_path, "tasks") }

    fn task_path(&self, id: &str) -> String { concat_path_leading_slash(&self.base_path, &format!("tasks/{id}")) }

    fn conflicts_path(&self) -> String { concat_path_leading_slash(&self.base_path, "conflicts/preview") }

    fn quota_path(&self) -> String { concat_path_leading_slash(&self.base_path, "quota") }

    fn rules_path(&self) -> String { concat_path_leading_slash(&self.base_path, "rules") }

    fn rule_path(&self, id: &str) -> String { concat_path_leading_slash(&self.base_path, &format!("rules/{id}")) }

    fn availability_path(&self) -> String { concat_path_leading_slash(&self.base_path, "availability") }

    /// GET /recording/tasks — list visible tasks.
    pub async fn list_tasks(&self) -> Result<RecordingSnapshot, RecordingError> {
        let body: RecordingSnapshot = request_get(&self.tasks_path(), None, Some(Encoding::Json))
            .await
            .map_err(network)?
            .ok_or_else(|| RecordingError::Other("empty response".into()))?;
        Ok(body)
    }

    /// POST /recording/tasks — create a recording. The request must
    /// carry source identifiers, never a free-form URL.
    pub async fn create_task(
        &self,
        request: CreateRecordingTaskRequest,
    ) -> Result<RecordingTaskResponse, RecordingError> {
        let body: RecordingTaskResponse = request_post(&self.tasks_path(), &request, None, Some(Encoding::Json))
            .await
            .map_err(network)?
            .ok_or_else(|| RecordingError::Other("empty response".into()))?;
        Ok(body)
    }

    /// PATCH /recording/tasks/{id}. Backend returns 204 No Content.
    pub async fn edit_task(&self, id: &str, request: EditRecordingTaskRequest) -> Result<(), RecordingError> {
        let _ = request_patch::<&EditRecordingTaskRequest, serde_json::Value>(
            &self.task_path(id),
            &request,
            None,
            Some(Encoding::Json),
        )
        .await
        .map_err(network)?;
        Ok(())
    }

    /// POST /recording/tasks/{id}/cancel

    /// POST /recording/tasks/{id}/pause
    pub async fn pause_task(&self, id: &str) -> Result<(), RecordingError> {
        let req = RecordingTaskId { id: id.to_string() };
        let _ = request_post::<&RecordingTaskId, serde_json::Value>(
            &format!("{}/pause", self.task_path(id)),
            &req,
            None,
            Some(Encoding::Json),
        )
        .await
        .map_err(network)?;
        Ok(())
    }

    /// POST /recording/tasks/{id}/resume
    pub async fn resume_task(&self, id: &str) -> Result<(), RecordingError> {
        let req = RecordingTaskId { id: id.to_string() };
        let _ = request_post::<&RecordingTaskId, serde_json::Value>(
            &format!("{}/resume", self.task_path(id)),
            &req,
            None,
            Some(Encoding::Json),
        )
        .await
        .map_err(network)?;
        Ok(())
    }

    /// POST /recording/tasks/{id}/retry
    pub async fn retry_task(&self, id: &str) -> Result<(), RecordingError> {
        let req = RecordingTaskId { id: id.to_string() };
        let _ = request_post::<&RecordingTaskId, serde_json::Value>(
            &format!("{}/retry", self.task_path(id)),
            &req,
            None,
            Some(Encoding::Json),
        )
        .await
        .map_err(network)?;
        Ok(())
    }

    /// DELETE /recording/tasks/{id}/remove
    pub async fn remove_task(&self, id: &str) -> Result<(), RecordingError> {
        let _ = request_delete::<serde_json::Value>(
            &format!("{}/remove", self.task_path(id)),
            None,
            Some(Encoding::Json),
        )
        .await
        .map_err(network)?;
        Ok(())
    }

    pub async fn cancel_task(&self, id: &str) -> Result<(), RecordingError> {
        let req = RecordingTaskId { id: id.to_string() };
        let _ = request_post::<&RecordingTaskId, serde_json::Value>(
            &format!("{}/cancel", self.task_path(id)),
            &req,
            None,
            Some(Encoding::Json),
        )
        .await
        .map_err(network)?;
        Ok(())
    }

    /// DELETE /recording/tasks/{id}
    pub async fn delete_task(&self, id: &str) -> Result<(), RecordingError> {
        request_delete::<()>(&self.task_path(id), None, None).await.map_err(network)?;
        Ok(())
    }

    /// POST /recording/conflicts/preview. Advisory only — the backend
    /// reads only the candidate fields plus `provider_scope`; `others`
    /// and `capacity` are server-derived or echoed from the request.
    pub async fn preview_conflicts(
        &self,
        request: &PreviewConflictsRequest,
    ) -> Result<RecordingConflictPreview, RecordingError> {
        let body: RecordingConflictPreview = request_post(&self.conflicts_path(), request, None, Some(Encoding::Json))
            .await
            .map_err(network)?
            .ok_or_else(|| RecordingError::Other("empty response".into()))?;
        Ok(body)
    }

    /// GET /recording/quota
    pub async fn get_quota(&self) -> Result<RecordingQuota, RecordingError> {
        let body: RecordingQuota = request_get(&self.quota_path(), None, Some(Encoding::Json))
            .await
            .map_err(network)?
            .ok_or_else(|| RecordingError::Other("empty response".into()))?;
        Ok(body)
    }

    /// GET /recording/rules
    pub async fn list_rules(&self) -> Result<Vec<RecordingRuleResponse>, RecordingError> {
        let body: Vec<RecordingRuleResponse> = request_get(&self.rules_path(), None, Some(Encoding::Json))
            .await
            .map_err(network)?
            .ok_or_else(|| RecordingError::Other("empty response".into()))?;
        Ok(body)
    }

    /// POST /recording/rules
    pub async fn create_rule(
        &self,
        request: CreateRecordingRuleRequest,
    ) -> Result<RecordingRuleResponse, RecordingError> {
        let body: RecordingRuleResponse = request_post(&self.rules_path(), &request, None, Some(Encoding::Json))
            .await
            .map_err(network)?
            .ok_or_else(|| RecordingError::Other("empty response".into()))?;
        Ok(body)
    }

    /// PATCH /recording/rules/{id}. Backend returns the full rule body.
    pub async fn edit_rule(
        &self,
        id: &str,
        request: EditRecordingRuleRequest,
    ) -> Result<RecordingRuleResponse, RecordingError> {
        let body: RecordingRuleResponse = request_patch(&self.rule_path(id), &request, None, Some(Encoding::Json))
            .await
            .map_err(network)?
            .ok_or_else(|| RecordingError::Other("empty response".into()))?;
        Ok(body)
    }

    /// DELETE /recording/rules/{id}?future=retain|cancel
    pub async fn delete_rule(&self, id: &str, future: &str) -> Result<(), RecordingError> {
        let path = format!("{}?future={future}", self.rule_path(id));
        request_delete::<()>(&path, None, None).await.map_err(network)?;
        Ok(())
    }

    /// GET /recording/availability — cheap preflight called before
    /// opening record forms. Returns `Ok(())` when the recording routes
    /// are reachable (DVR enabled) and a typed `RecordingError`
    /// otherwise; the `Disabled` variant maps to the actionable i18n
    /// message that points operators at the Video > Download toggle.
    pub async fn ensure_available(&self) -> Result<(), RecordingError> {
        let _body: Option<serde_json::Value> =
            request_get(&self.availability_path(), None, Some(Encoding::Json)).await.map_err(network)?;
        Ok(())
    }
}

fn network(e: Error) -> RecordingError {
    // A network error here means the call did not reach a
    // permission-aware response. Surface it as a generic `Network`
    // variant; the UI layer decides whether to retry, refresh, or
    // show a connection error. Do NOT collapse it into
    // `TokenRefreshRequired` — the auth layer is the source of
    // truth for that decision.
    if let Some(code) = extract_error_code(&e) {
        return RecordingError::from_code(&code);
    }
    match e {
        Error::RequestError => RecordingError::Network("request error".into()),
        other => RecordingError::Network(other.to_string()),
    }
}

/// Try to pull a stable backend error code (e.g. `recording_forbidden`,
/// `recording_quota_exceeded`) out of the `Error` payload. The
/// `request` crate embeds the response body on non-2xx replies; the
/// request layer parses JSON error bodies into `Error::{...}(message)`
/// where `message` is the `error` field. So the stable code is the
/// message itself when it matches our wire format.
fn extract_error_code(e: &Error) -> Option<String> {
    let body = e.to_string();
    if body.starts_with("recording_") {
        Some(body)
    } else {
        None
    }
}

impl Default for RecordingService {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn create_recording_task_serializes_stable_target_name() {
        let request = CreateRecordingTaskRequest {
            source: RecordingSourceInput {
                target_id: "default".to_string(),
                virtual_id: "42".to_string(),
                cluster: XtreamCluster::Live,
                input_name: "input-a".to_string(),
            },
            program_title: "News".to_string(),
            program_start: Some(100),
            program_end: Some(200),
            pre_roll_secs: Some(0),
            post_roll_secs: Some(0),
            visibility: "private".to_string(),
            channel_id: None,
            channel_name: None,
            epg: None,
        };

        assert_eq!(
            serde_json::to_value(request).expect("serialize recording task"),
            json!({
                "source": {
                    "target_id": "default",
                    "virtual_id": "42",
                    "cluster": "Live",
                    "input_name": "input-a"
                },
                "program_title": "News",
                "program_start": 100,
                "program_end": 200,
                "pre_roll_secs": 0,
                "post_roll_secs": 0,
                "visibility": "private"
            })
        );
    }

    #[test]
    fn create_weekly_rule_serializes_target_name_and_body() {
        let request = CreateRecordingRuleRequest {
            target_id: "default".to_string(),
            virtual_id: "42".to_string(),
            input_name: "input-a".to_string(),
            body: shared::model::recording_rule::RuleBody::WeeklyTimeslot {
                weekday: 3,
                local_start_time: "20:00".to_string(),
                duration_secs: 3600,
                timezone: "Europe/Berlin".to_string(),
            },
            channel_id: None,
            pre_roll_secs: 60,
            post_roll_secs: 120,
            visibility: RuleVisibility::Private,
        };

        assert_eq!(
            serde_json::to_value(request).expect("serialize weekly rule"),
            json!({
                "target_id": "default",
                "virtual_id": "42",
                "input_name": "input-a",
                "body": {
                    "kind": "weekly_timeslot",
                    "weekday": 3,
                    "local_start_time": "20:00",
                    "duration_secs": 3600,
                    "timezone": "Europe/Berlin"
                },
                "channel_id": null,
                "pre_roll_secs": 60,
                "post_roll_secs": 120,
                "visibility": "private"
            })
        );
    }

    #[test]
    fn create_new_episode_rule_serializes_body() {
        let request = CreateRecordingRuleRequest {
            target_id: "default".to_string(),
            virtual_id: "42".to_string(),
            input_name: "input-a".to_string(),
            body: shared::model::recording_rule::RuleBody::NewEpisode {
                series_id: Some("series-1".to_string()),
                title_pattern: None,
                exclude_repeat: true,
            },
            channel_id: Some("channel-1".to_string()),
            pre_roll_secs: 0,
            post_roll_secs: 0,
            visibility: RuleVisibility::Shared,
        };

        assert_eq!(
            serde_json::to_value(request).expect("serialize new episode rule"),
            json!({
                "target_id": "default",
                "virtual_id": "42",
                "input_name": "input-a",
                "body": {
                    "kind": "new_episode",
                    "series_id": "series-1",
                    "title_pattern": null,
                    "exclude_repeat": true
                },
                "channel_id": "channel-1",
                "pre_roll_secs": 0,
                "post_roll_secs": 0,
                "visibility": "shared"
            })
        );
    }

    #[test]
    fn edit_rule_serializes_optional_body() {
        let request = EditRecordingRuleRequest {
            body: Some(shared::model::recording_rule::RuleBody::NewEpisode {
                series_id: None,
                title_pattern: Some("News".to_string()),
                exclude_repeat: false,
            }),
            channel_id: None,
            pre_roll_secs: None,
            post_roll_secs: None,
            visibility: None,
            enabled: None,
            clear_channel_id: false,
        };

        assert_eq!(
            serde_json::to_value(request).expect("serialize edit rule"),
            json!({
                "body": {
                    "kind": "new_episode",
                    "series_id": null,
                    "title_pattern": "News",
                    "exclude_repeat": false
                }
            })
        );
    }

    #[test]
    fn edit_rule_serializes_explicit_channel_clear() {
        let request = EditRecordingRuleRequest {
            body: None,
            channel_id: None,
            clear_channel_id: true,
            pre_roll_secs: None,
            post_roll_secs: None,
            visibility: None,
            enabled: None,
        };

        assert_eq!(serde_json::to_value(request).expect("serialize channel clear"), json!({"clear_channel_id": true}));
    }

    #[test]
    fn error_from_code_maps_known_codes() {
        assert_eq!(RecordingError::from_code("recording_invalid_source"), RecordingError::InvalidSource);
        assert_eq!(
            RecordingError::from_code("recording_shared_not_administrator"),
            RecordingError::SharedCreationNotAdministrator
        );
        assert_eq!(RecordingError::from_code("recording_forbidden"), RecordingError::Forbidden);
        assert_eq!(RecordingError::from_code("recording_unknown"), RecordingError::UnknownRecording);
        assert_eq!(RecordingError::from_code("recording_duplicate"), RecordingError::Duplicate);
        assert_eq!(RecordingError::from_code("recording_quota_exceeded"), RecordingError::QuotaExceeded);
        assert_eq!(
            RecordingError::from_code("recording_path_reservation_failed"),
            RecordingError::PathReservationFailed
        );
        assert_eq!(RecordingError::from_code("recording_invalid_state"), RecordingError::InvalidState);
        assert_eq!(RecordingError::from_code("recording_invalid_interval"), RecordingError::InvalidInterval);
    }

    #[test]
    fn http_response_recording_code_maps_to_typed_error() {
        assert_eq!(network(Error::HttpResponse("recording_disabled".to_string())), RecordingError::Disabled);
    }

    #[test]
    fn error_from_code_triggers_token_refresh_for_schema_errors() {
        // Missing subject_id or a stale permission schema_version both
        // trigger token-refresh. We surface both via the same enum
        // variant.
        assert_eq!(RecordingError::from_code("recording_unknown_owner"), RecordingError::TokenRefreshRequired);
        assert_eq!(RecordingError::from_code("recording_invalid_template"), RecordingError::TokenRefreshRequired);
    }

    #[test]
    fn error_from_code_falls_through_for_unknown() {
        assert!(matches!(RecordingError::from_code("recording_brand_new_code"), RecordingError::Other(_)));
    }

    #[test]
    fn error_code_is_stable() {
        // The error codes are part of the wire contract and must not
        // change without a coordinated i18n update.
        assert_eq!(RecordingError::TokenRefreshRequired.code(), "recording_token_refresh_required");
        assert_eq!(RecordingError::InvalidSource.code(), "recording_invalid_source");
        assert_eq!(RecordingError::SharedCreationNotAdministrator.code(), "recording_shared_not_administrator");
        assert_eq!(RecordingError::Forbidden.code(), "recording_forbidden");
        assert_eq!(RecordingError::PathReservationFailed.code(), "recording_path_reservation_failed");
    }

    #[test]
    fn paths_include_api_v1_recording_prefix() {
        let svc = RecordingService::with_base_path("/api/v1/recording".to_string());
        assert_eq!(svc.tasks_path(), "/api/v1/recording/tasks");
        assert_eq!(svc.task_path("rec-1"), "/api/v1/recording/tasks/rec-1");
        assert_eq!(svc.conflicts_path(), "/api/v1/recording/conflicts/preview");
        assert_eq!(svc.quota_path(), "/api/v1/recording/quota");
        assert_eq!(svc.rules_path(), "/api/v1/recording/rules");
        assert_eq!(svc.rule_path("rule-1"), "/api/v1/recording/rules/rule-1");
        assert_eq!(svc.availability_path(), "/api/v1/recording/availability");
    }

    #[test]
    fn preview_conflicts_request_serializes_to_backend_shape() {
        let req = PreviewConflictsRequest {
            source: PreviewSourceDto {
                target_name: "target-1".to_string(),
                virtual_id: "42".to_string(),
                input_name: "input-a".to_string(),
            },
            candidate: PreviewCandidateDto {
                padded_start: 1_700_000_000,
                padded_end: 1_700_003_600,
                pre_roll_secs: 0,
                post_roll_secs: 0,
                priority: 5,
            },
        };
        let json = serde_json::to_string(&req).expect("serialize");
        assert!(json.contains("\"source\""));
        assert!(json.contains("\"target_name\":\"target-1\""));
        assert!(json.contains("\"candidate\""));
        // The client never submits `others`, `capacity`, or
        // `provider_scope`; the server derives them from the queue
        // state and config.
        assert!(!json.contains("\"others\""));
        assert!(!json.contains("\"capacity\""));
        assert!(!json.contains("\"background_slots\""));
        assert!(!json.contains("\"provider_scope\""));
        assert!(json.contains("\"padded_start\":1700000000"));
    }

    #[test]
    fn preview_conflicts_response_deserializes_severity_snake_case() {
        let json = r#"{"severity":"possible_capacity_wait","provider_scope":"de","overlap_segments":[]}"#;
        let parsed: RecordingConflictPreview = serde_json::from_str(json).expect("deserialize");
        assert_eq!(parsed.severity, ConflictSeverity::PossibleCapacityWait);
        assert_eq!(parsed.provider_scope.as_deref(), Some("de"));
    }
}
