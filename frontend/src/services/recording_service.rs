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
    services::{get_base_href, request_delete, request_get, request_post, request_put, Encoding},
};
use serde::{Deserialize, Serialize};
use shared::{model::XtreamCluster, utils::concat_path_leading_slash};

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
    pub program_start: i64,
    pub program_end: i64,
    pub pre_roll_secs: u64,
    pub post_roll_secs: u64,
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

#[derive(Clone, Debug, Deserialize)]
pub struct RecordingTaskResponse {
    pub id: String,
    pub title: String,
    pub recording: Option<shared::model::recording::RecordingTaskDto>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RecordingSnapshot {
    pub revision: u64,
    pub tasks: Vec<RecordingTaskResponse>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RecordingConflict {
    pub task_id: String,
    pub revision: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RecordingConflictPreview {
    pub conflicts: Vec<RecordingConflict>,
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
    pub owner_id: String,
    pub target_id: String,
    pub virtual_id: String,
    pub input_name: String,
    pub weekday: u8,
    pub start_time: String,
    pub duration_secs: u64,
    pub channel_id: Option<String>,
    pub pre_roll_secs: u64,
    pub post_roll_secs: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct EditRecordingRuleRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weekday: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_roll_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post_roll_secs: Option<u64>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RecordingRuleResponse {
    pub id: String,
    pub revision: u64,
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
    /// Completed recording, a foreign relative_path, or a missing
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
            "recording_unknown_owner" | "recording_invalid_template" => Self::TokenRefreshRequired,
            "recording_invalid_source" => Self::InvalidSource,
            "recording_invalid_path" => Self::InvalidPath,
            "recording_shared_not_administrator" => Self::SharedCreationNotAdministrator,
            "recording_forbidden" => Self::Forbidden,
            "recording_invalid_state" => Self::InvalidState,
            "recording_invalid_interval" => Self::InvalidInterval,
            "recording_invalid_padding" => Self::InvalidInterval,
            "recording_unknown" => Self::UnknownRecording,
            "recording_duplicate" => Self::Duplicate,
            "recording_path_reservation_failed" => Self::PathReservationFailed,
            "recording_quota_exceeded" => Self::QuotaExceeded,
            "recording_io_error" | "recording_persistence_failed" => Self::Other(code.to_string()),
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
            Self::Other(_) => "recording_other",
            Self::Network(_) => "recording_network_error",
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

    /// PATCH /recording/tasks/{id}
    pub async fn edit_task(
        &self,
        id: &str,
        request: EditRecordingTaskRequest,
    ) -> Result<RecordingTaskResponse, RecordingError> {
        let body: RecordingTaskResponse = request_put(&self.task_path(id), &request, None, Some(Encoding::Json))
            .await
            .map_err(network)?
            .ok_or_else(|| RecordingError::Other("empty response".into()))?;
        Ok(body)
    }

    /// POST /recording/tasks/{id}/cancel
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
        let _ = {
            let path = format!("{}?uuid={id}", self.task_path(id));
            request_delete::<()>(&path, None, None).await.map_err(network)?
        };
        Ok(())
    }

    /// POST /recording/conflicts/preview
    pub async fn preview_conflicts(
        &self,
        request: &CreateRecordingTaskRequest,
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

    /// PATCH /recording/rules/{id}
    pub async fn edit_rule(
        &self,
        id: &str,
        request: EditRecordingRuleRequest,
    ) -> Result<RecordingRuleResponse, RecordingError> {
        let body: RecordingRuleResponse = request_put(&self.rule_path(id), &request, None, Some(Encoding::Json))
            .await
            .map_err(network)?
            .ok_or_else(|| RecordingError::Other("empty response".into()))?;
        Ok(body)
    }

    /// DELETE /recording/rules/{id}?future=retain|cancel
    pub async fn delete_rule(&self, id: &str, future: &str) -> Result<(), RecordingError> {
        let _ = {
            let path = format!("{}?future={future}&id={id}", self.rule_path(id));
            request_delete::<()>(&path, None, None).await.map_err(network)?
        };
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
/// `request` crate embeds the response body on non-2xx replies; for
/// our endpoints the body is JSON of shape `{"error": "<code>"}`.
fn extract_error_code(e: &Error) -> Option<String> {
    let body = e.to_string();
    let start = body.find("\"error\"")?;
    let after = body.get(start..)?;
    let colon = after.find(':')?;
    let rest = after.get(colon + 1..)?;
    let quote1 = rest.find('"')?;
    let rest = rest.get(quote1 + 1..)?;
    let quote2 = rest.find('"')?;
    rest.get(..quote2).map(|s| s.to_string())
}

impl Default for RecordingService {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
